//! Share screen between net services: real iroh endpoints and a relay on loopback, the share
//! worker running in-process on the test pattern and the test codec, so nothing needs a screen,
//! a GPU or a game.
//!
//! Each node is what the net service builds when a player is in a party: the tunnels' endpoint
//! and a screen hub that knows the other members.

mod support;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyplay_control::{Link, ScreenPhase, ScreenSource};
use anyplay_net::screen::{
    self, GameWindow, Hub, Member, Members, Output, Refusal, View, WorkerPort, Workers,
};
use anyplay_net::traffic::Traffic;
use anyplay_net::tunnel::{Options, RelayAccess, Tunnels};
use anyplay_share::codec::{Frame, VideoDecoder};
use anyplay_share::control::Role;
use anyplay_share::link::{Capabilities, FromWorker};
use anyplay_share::testcodec::{TestDecoder, TestEncoder, TestPattern};
use anyplay_share_worker::host::{self, PatternSource};
use anyplay_share_worker::viewer::{self, DecoderFactory, FrameSink};
use anyplay_share_worker::{Port, test_codec_caps};
use iroh::endpoint::ConnectionError;
use iroh::{EndpointAddr, EndpointId, SecretKey};
use tokio::sync::mpsc;

/// What a viewer's window was given.
#[derive(Default)]
struct Shown {
    /// The frame number read back from each picture, in the order shown.
    numbers: Vec<u32>,
    /// Pictures that were not exactly the test picture of their number.
    damaged: u32,
}

struct CheckSink(Arc<Mutex<Shown>>);

impl FrameSink for CheckSink {
    fn show(&mut self, frame: &Frame) {
        let mut shown = self.0.lock().unwrap();
        match TestPattern::read_number(frame) {
            Some(number)
                if TestPattern::render(frame.width, frame.height, number).bgra == frame.bgra =>
            {
                shown.numbers.push(number)
            }
            _ => shown.damaged += 1,
        }
    }
}

/// Workers that run in threads of the test.
struct TestWorkers {
    shown: Arc<Mutex<Shown>>,
}

impl Workers for TestWorkers {
    fn capabilities(&self) -> Option<Capabilities> {
        Some(Capabilities {
            encode: vec![test_codec_caps()],
            decode: vec![test_codec_caps()],
            capture: false,
            libraries: Vec::new(),
        })
    }

    fn launch(
        &self,
        role: Role,
        source: ScreenSource,
        _: &str,
        _: &GameWindow,
    ) -> Result<WorkerPort, String> {
        if role == Role::Host {
            assert_eq!(
                source,
                ScreenSource::TestPattern,
                "tests stream the pattern"
            );
        }
        let (to_tx, mut to_rx) = anyplay_net::worker_process::to_worker_channel();
        let (from_tx, from_rx) = mpsc::channel::<FromWorker>(4096);
        let (worker_in_tx, worker_in) = std::sync::mpsc::channel();
        let (worker_out, worker_out_rx) = std::sync::mpsc::channel::<FromWorker>();
        std::thread::spawn(move || {
            while let Some(message) = to_rx.blocking_recv() {
                if worker_in_tx.send(message).is_err() {
                    return;
                }
            }
        });
        std::thread::spawn(move || {
            while let Ok(message) = worker_out_rx.recv() {
                if from_tx.blocking_send(message).is_err() {
                    return;
                }
            }
        });
        let port = Port {
            from_service: worker_in,
            to_service: worker_out,
        };
        let shown = self.shown.clone();
        std::thread::spawn(move || match role {
            Role::Host => host::run(
                Box::new(PatternSource::default()),
                Box::new(TestEncoder::new(60)),
                port,
            ),
            Role::Guest => {
                let decoders: DecoderFactory = Box::new(|codec| match codec {
                    "test" => Some(Box::new(TestDecoder::new()) as Box<dyn VideoDecoder>),
                    _ => None,
                });
                viewer::run(decoders, Box::new(CheckSink(shown)), port)
            }
        });
        Ok(WorkerPort {
            to: to_tx,
            from: from_rx,
        })
    }
}

/// A PC without a share worker.
struct NoWorkers;

impl Workers for NoWorkers {
    fn capabilities(&self) -> Option<Capabilities> {
        None
    }

    fn launch(
        &self,
        _: Role,
        _: ScreenSource,
        _: &str,
        _: &GameWindow,
    ) -> Result<WorkerPort, String> {
        Err("no worker".into())
    }
}

struct Node {
    account: u64,
    tunnels: Tunnels,
    hub: Option<Hub>,
    shown: Arc<Mutex<Shown>>,
    views: Arc<Mutex<Vec<View>>>,
    notices: Arc<Mutex<Vec<String>>>,
}

async fn node(relay: &support::relay::TestRelay, account: u64) -> Node {
    let traffic = Traffic::start().await.unwrap();
    let (changes, _) = mpsc::unbounded_channel();
    let tunnels = Tunnels::start(
        Options {
            secret: SecretKey::generate(),
            relays: vec![RelayAccess {
                url: relay.url.clone(),
                token: "ticket".into(),
            }],
            relay_roots: vec![relay.root.clone()],
            bind: Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
            relay_only: false,
            relay_quic_port: None,
        },
        traffic,
        changes,
    )
    .await
    .unwrap();
    assert!(
        tunnels.online(Duration::from_secs(10)).await,
        "relay not reached"
    );
    Node {
        account,
        tunnels,
        hub: None,
        shown: Arc::new(Mutex::new(Shown::default())),
        views: Arc::new(Mutex::new(Vec::new())),
        notices: Arc::new(Mutex::new(Vec::new())),
    }
}

impl Node {
    fn id(&self) -> EndpointId {
        self.tunnels.id()
    }

    /// What the party tells every member about this one.
    fn as_member(&self) -> (EndpointId, Member) {
        (
            self.id(),
            Member {
                account: self.account,
                relay: self.tunnels.home_relay(),
            },
        )
    }

    /// Is in a party with `others`: the hub runs and knows them.
    fn join(&mut self, others: &[&Node], workers: Option<Arc<dyn Workers>>) {
        let members: Members = others.iter().map(|n| n.as_member()).collect();
        let (out, mut rx) = mpsc::unbounded_channel();
        let (views, notices) = (self.views.clone(), self.notices.clone());
        tokio::spawn(async move {
            while let Some(output) = rx.recv().await {
                match output {
                    Output::View(view) => views.lock().unwrap().push(view),
                    Output::Notice { message, .. } => notices.lock().unwrap().push(message),
                }
            }
        });
        let workers = workers.unwrap_or_else(|| {
            Arc::new(TestWorkers {
                shown: self.shown.clone(),
            })
        });
        self.hub = Some(Hub::start(&self.tunnels, workers, members, out));
    }

    fn hub(&self) -> &Hub {
        self.hub.as_ref().expect("joined")
    }

    fn view(&self) -> View {
        self.views
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default()
    }

    fn shown(&self) -> usize {
        self.shown.lock().unwrap().numbers.len()
    }
}

async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_watches_the_stream_and_every_picture_is_whole() {
    let relay = support::relay::start().await;
    let mut a = node(&relay, 1).await;
    let mut b = node(&relay, 2).await;
    let mut c = node(&relay, 3).await;
    a.join(&[&b, &c], None);
    b.join(&[&a, &c], None);
    c.join(&[&a, &b], None);

    a.hub()
        .start_sharing("Astro".into(), ScreenSource::TestPattern)
        .await
        .expect("start sharing");
    // Both members are offered the stream, and nothing flows yet.
    eventually("b and c were not offered the stream", || {
        [&b, &c].iter().all(|n| {
            n.view()
                .offers
                .iter()
                .any(|o| o.account.0 == 1 && o.title == "Astro")
        })
    })
    .await;
    assert_eq!(b.shown(), 0);
    assert!(
        a.view()
            .hosting
            .is_some_and(|h| h.phase == ScreenPhase::Connecting && h.codec.is_none()),
        "no worker runs while nobody watches"
    );

    // b watches; c does not.
    b.hub().watch(1).await.expect("watch");
    eventually("b never saw a picture", || {
        b.shown() >= 30
            && b.view()
                .watching
                .is_some_and(|w| w.phase == ScreenPhase::Live)
    })
    .await;
    {
        let shown = b.shown.lock().unwrap();
        assert_eq!(shown.damaged, 0, "a damaged picture was shown");
        assert!(
            shown.numbers.windows(2).all(|w| w[0] < w[1]),
            "pictures out of order"
        );
    }
    assert_eq!(c.shown(), 0, "c did not choose to watch and got video");

    // The streamer sees who watches, the path and the round trip.
    eventually("a never saw b watch live", || {
        a.view().hosting.is_some_and(|h| {
            h.phase == ScreenPhase::Live
                && h.codec.as_deref() == Some("test")
                && h.viewers.iter().any(|v| {
                    v.account.0 == 2
                        && v.watching
                        && v.link.is_some_and(|l| l != Link::Connecting)
                        && v.rtt_ms.is_some()
                })
                && h.viewers.iter().any(|v| v.account.0 == 3 && !v.watching)
        })
    })
    .await;
    let watching = b.view().watching.expect("watching");
    assert_eq!(watching.title, "Astro");
    assert!(watching.link.is_some_and(|l| l != Link::Connecting));

    // A second watch at once is refused; so is watching what nobody offers.
    assert_eq!(b.hub().watch(1).await, Err(Refusal::Busy));
    assert_eq!(c.hub().watch(7).await, Err(Refusal::NotFound));

    // b leaves: the offer is back, the picture stops, and a's worker ends with the last viewer.
    b.hub().leave();
    eventually("b did not leave", || {
        let view = b.view();
        view.watching.is_none() && view.offers.iter().any(|o| o.account.0 == 1)
    })
    .await;
    eventually("a still has a watcher", || {
        a.view()
            .hosting
            .is_some_and(|h| h.viewers.iter().all(|v| !v.watching) && h.codec.is_none())
    })
    .await;

    // c joins the running stream late and gets a whole picture from a key frame on.
    // (Pictures already on their way to b when it left may still be counted; let them settle.)
    tokio::time::sleep(Duration::from_millis(300)).await;
    let before = b.shown();
    c.hub().watch(1).await.expect("c watches");
    eventually("c never saw a picture", || c.shown() >= 20).await;
    assert_eq!(c.shown.lock().unwrap().damaged, 0);
    assert_eq!(b.shown(), before, "b got pictures after leaving");

    // The streamer stops: the offers go, the watching ends.
    a.hub().stop_sharing("ended");
    eventually("the stream did not end for c", || {
        let view = c.view();
        view.watching.is_none() && view.offers.is_empty()
    })
    .await;
    eventually("the stream did not end for a", || {
        a.view().hosting.is_none()
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_game_ending_ends_the_stream_for_the_viewers() {
    let relay = support::relay::start().await;
    let mut a = node(&relay, 1).await;
    let mut b = node(&relay, 2).await;
    a.join(&[&b], None);
    b.join(&[&a], None);
    a.hub()
        .start_sharing("Astro".into(), ScreenSource::TestPattern)
        .await
        .unwrap();
    eventually("no offer", || !b.view().offers.is_empty()).await;
    b.hub().watch(1).await.unwrap();
    eventually("no picture", || b.shown() >= 5).await;
    a.hub().stop_sharing("game_ended");
    eventually("b is still watching", || {
        let view = b.view();
        view.watching.is_none() && view.offers.is_empty()
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn someone_who_is_not_a_member_is_refused() {
    let relay = support::relay::start().await;
    let mut a = node(&relay, 1).await;
    let mut b = node(&relay, 2).await;
    let stranger = node(&relay, 9).await;
    a.join(&[&b], None);
    b.join(&[&a], None);

    // The stranger knows b's key and relay, as any friend of b's may, and dials the screen ALPN
    // to offer a stream.
    let address = EndpointAddr::new(b.id()).with_relay_url(b.tunnels.home_relay().unwrap());
    let connection = stranger
        .tunnels
        .endpoint()
        .connect(address, screen::ALPN)
        .await
        .expect("the handshake itself is open to everyone");
    let closed = tokio::time::timeout(Duration::from_secs(10), connection.closed())
        .await
        .expect("b never hung up on the stranger");
    assert!(
        matches!(&closed, ConnectionError::ApplicationClosed(c) if u64::from(c.error_code) == 1),
        "{closed:?}"
    );
    assert!(b.view().offers.is_empty(), "the stranger became an offer");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_a_worker_nothing_starts() {
    let relay = support::relay::start().await;
    let mut a = node(&relay, 1).await;
    let b = node(&relay, 2).await;
    a.join(&[&b], Some(Arc::new(NoWorkers)));
    assert_eq!(
        a.hub()
            .start_sharing("Astro".into(), ScreenSource::Game)
            .await,
        Err(Refusal::NoEncoder)
    );
    assert_eq!(a.hub().watch(2).await, Err(Refusal::NotFound));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leaving_the_party_ends_the_connection() {
    let relay = support::relay::start().await;
    let mut a = node(&relay, 1).await;
    let mut b = node(&relay, 2).await;
    a.join(&[&b], None);
    b.join(&[&a], None);
    a.hub()
        .start_sharing("Astro".into(), ScreenSource::TestPattern)
        .await
        .unwrap();
    eventually("no offer", || !b.view().offers.is_empty()).await;
    // b's party no longer has a.
    b.hub().set_members(Members::new());
    eventually("the offer stayed after a left the party", || {
        b.view().offers.is_empty()
    })
    .await;
}
