//! Share play between two net services: real iroh endpoints and a relay on loopback, the
//! host's bridge listener with the fake game behind it, the share workers running in-process on
//! the frame ring and the test codec, so nothing needs a screen, a GPU or a real game.
//!
//! Each node is what the net service builds while a player is signed in: the tunnels' endpoint
//! and a share play hub. The tests bind the hubs the way the service does once the cloud's
//! `Share` is active, then play: the guest sees the game's pictures, its pad states reach the
//! fake game on the slot of the mode, and every way the share can end ends it.

mod support;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyplay_api::{ShareMode, SharePath};
use anyplay_bridge_protocol::{Frame, Message, OverrideAction, VERSION, Welcome};
use anyplay_control::{Link, ShareStats};
use anyplay_fake_game::Bridge;
use anyplay_net::bridge::{self, BridgeInput};
use anyplay_net::play::{self, Bind, Hub, Output, Phase, View};
use anyplay_net::screen::{PlayLaunch, WorkerPort, Workers};
use anyplay_net::traffic::Traffic;
use anyplay_net::tunnel::{Options, RelayAccess, Tunnels};
use anyplay_share::codec::{Frame as Picture, VideoDecoder, VideoEncoder};
use anyplay_share::control::{Report, Role};
use anyplay_share::link::{Capabilities, FromWorker, Start, ToWorker};
use anyplay_share::pad::{PadState, button};
use anyplay_share::ring::RingWriter;
use anyplay_share::testcodec::{TestDecoder, TestEncoder, TestPattern};
use anyplay_share_worker::shm::Shm;
use anyplay_share_worker::viewer::{self, DecoderFactory, FrameSink};
use anyplay_share_worker::{Port, host, ring_source, test_codec_caps};
use iroh::endpoint::ConnectionError;
use iroh::{EndpointAddr, EndpointId, SecretKey};
use tokio::sync::mpsc;

const WIDTH: u32 = 160;
const HEIGHT: u32 = 90;

/// What a viewer's window was given.
#[derive(Default)]
struct Shown {
    numbers: Vec<u32>,
    damaged: u32,
}

struct CheckSink(Arc<Mutex<Shown>>);

impl FrameSink for CheckSink {
    fn show(&mut self, frame: &Picture) {
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
#[derive(Default)]
struct TestWorkers {
    shown: Arc<Mutex<Shown>>,
    /// The launches, in order.
    launches: Mutex<Vec<PlayLaunch>>,
    /// Where the guest's viewer would put the pad it read.
    pad: Mutex<Option<std::sync::mpsc::Sender<FromWorker>>>,
    /// Every setup the net service gave a worker, with the role of the worker.
    setups: Mutex<Vec<(Role, Start)>>,
}

impl TestWorkers {
    /// The setups a role's worker was given, in order.
    fn setups(&self, role: Role) -> Vec<Start> {
        self.setups
            .lock()
            .unwrap()
            .iter()
            .filter(|(r, _)| *r == role)
            .map(|(_, start)| start.clone())
            .collect()
    }

    /// The guest's viewer reports: `lost` of every `received + lost` packets went missing.
    fn report(&self, received: u32, lost: u32) {
        let pad = self.pad.lock().unwrap();
        pad.as_ref()
            .expect("a guest worker runs")
            .send(FromWorker::Report(Report {
                frame: 0,
                received,
                lost,
                recovered: 0,
                delay_trend_us: 0,
                queue_ms: 0,
                decoded_fps: 30,
                shown_fps: 30,
            }))
            .unwrap();
    }

    fn press(&self, buttons: u32) {
        let pad = self.pad.lock().unwrap();
        pad.as_ref()
            .expect("a guest worker runs")
            .send(FromWorker::Pad(PadState {
                buttons,
                ..PadState::NEUTRAL
            }))
            .unwrap();
    }
}

struct Shared(Arc<TestWorkers>);

impl Workers for Shared {
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
        _: Role,
        _: anyplay_control::ScreenSource,
        _: &str,
        _: &anyplay_net::screen::GameWindow,
    ) -> Result<WorkerPort, String> {
        Err("share play launches its own".into())
    }

    fn launch_play(&self, launch: &PlayLaunch) -> Result<WorkerPort, String> {
        self.0.launches.lock().unwrap().push(launch.clone());
        let role = launch.role;
        let ring = launch.ring.clone();
        let (to_tx, mut to_rx) = anyplay_net::worker_process::to_worker_channel();
        let (from_tx, from_rx) = mpsc::channel::<FromWorker>(4096);
        let (worker_in_tx, worker_in) = std::sync::mpsc::channel();
        let (worker_out, worker_out_rx) = std::sync::mpsc::channel::<FromWorker>();
        if role == Role::Guest {
            *self.0.pad.lock().unwrap() = Some(worker_out.clone());
        }
        let workers = self.0.clone();
        std::thread::spawn(move || {
            while let Some(message) = to_rx.blocking_recv() {
                if let ToWorker::Start(start) = &message {
                    workers.setups.lock().unwrap().push((role, start.clone()));
                }
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
        let shown = self.0.shown.clone();
        std::thread::spawn(move || match role {
            Role::Host => {
                let ring = ring.expect("the host's worker makes the ring");
                let (frames, sound, ready) =
                    ring_source::create(&ring.name, WIDTH, HEIGHT, 60).expect("the ring");
                let _ = port.to_service.send(FromWorker::RingReady(ready));
                host::run_with_sound(
                    Box::new(frames),
                    Box::new(|_| Ok(Box::new(TestEncoder::new(60)) as Box<dyn VideoEncoder>)),
                    Some(Box::new(sound)),
                    port,
                );
            }
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

/// What a hub reported, in order.
#[derive(Debug)]
enum Seen {
    View(Option<View>),
    Mode(ShareMode),
    Request(ShareMode),
    Path(SharePath),
    Ended(String, String),
    Notice(String),
}

struct Node {
    tunnels: Tunnels,
    hub: Option<Hub>,
    workers: Arc<TestWorkers>,
    seen: Arc<Mutex<Vec<Seen>>>,
    /// Where the hub's frames for the game go, once a game is connected.
    to_game: Arc<Mutex<Option<mpsc::UnboundedSender<Frame>>>>,
}

async fn node(relay: &support::relay::TestRelay) -> Node {
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
    let mut node = Node {
        tunnels,
        hub: None,
        workers: Arc::new(TestWorkers::default()),
        seen: Arc::new(Mutex::new(Vec::new())),
        to_game: Arc::new(Mutex::new(None)),
    };
    node.start_hub();
    node
}

impl Node {
    fn id(&self) -> EndpointId {
        self.tunnels.id()
    }

    fn start_hub(&mut self) {
        let (out, mut rx) = mpsc::unbounded_channel();
        let (seen, to_game) = (self.seen.clone(), self.to_game.clone());
        tokio::spawn(async move {
            while let Some(output) = rx.recv().await {
                let entry = match output {
                    Output::View(view) => Seen::View(view),
                    Output::Bridge(frame) => {
                        if let Some(game) = to_game.lock().unwrap().as_ref() {
                            let _ = game.send(frame);
                        }
                        continue;
                    }
                    Output::Mode(mode) => Seen::Mode(mode),
                    Output::Request(mode) => Seen::Request(mode),
                    Output::Path(path) => Seen::Path(path),
                    Output::Ended { id, reason } => Seen::Ended(id, reason),
                    Output::Notice { message, .. } => Seen::Notice(message),
                };
                seen.lock().unwrap().push(entry);
            }
        });
        self.hub = Some(Hub::start(
            &self.tunnels,
            Arc::new(Shared(self.workers.clone())),
            out,
        ));
    }

    fn hub(&self) -> &Hub {
        self.hub.as_ref().expect("a hub")
    }

    fn bind(&self, role: Role, peer: &Node, mode: ShareMode) {
        self.hub().bind(Bind {
            id: "s1".into(),
            role,
            peer: peer.id(),
            relay: peer.tunnels.home_relay(),
            title: "Astro".into(),
            mode,
        });
    }

    fn view(&self) -> Option<View> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find_map(|s| match s {
                Seen::View(view) => Some(view.clone()),
                _ => None,
            })
            .flatten()
    }

    fn saw(&self, what: impl Fn(&Seen) -> bool) -> bool {
        self.seen.lock().unwrap().iter().any(what)
    }
}

/// A game on the host's PC: connected to a bridge listener, with the net service's side wired to
/// the host's hub and the game's pictures written into the ring it was asked for.
struct Game {
    game: Arc<Bridge>,
}

impl Game {
    async fn start(host: &Node) -> Game {
        let (tx, mut inputs) = mpsc::unbounded_channel();
        let port = bridge::listen(tx).await.expect("the listener");
        let endpoint = format!("127.0.0.1:{port}");
        let connecting = tokio::spawn(async move { Bridge::connect(&endpoint, "0a0b0c").await });
        let BridgeInput::Hello {
            frames,
            verdict,
            token,
            ..
        } = inputs.recv().await.expect("a hello")
        else {
            panic!("the first input is the hello");
        };
        assert_eq!(token, [0x0a, 0x0b, 0x0c]);
        let welcome = Message::Welcome(Welcome {
            version: VERSION,
            udp_port: 0,
            tcp_port: 0,
            stream_key: [0; 16],
        });
        frames.send(Frame::event(&welcome).unwrap()).unwrap();
        verdict.send(Ok(())).unwrap();
        let game = connecting.await.unwrap().expect("the game connects");
        *host.to_game.lock().unwrap() = Some(frames);
        // What the game sends goes to the hub, as the service does.
        let hub_ctl = host.hub().handle();
        tokio::spawn(async move {
            while let Some(input) = inputs.recv().await {
                match input {
                    BridgeInput::Frame { frame, .. } => hub_ctl.bridge(frame),
                    BridgeInput::Closed { .. } => hub_ctl.bridge_lost(),
                    BridgeInput::Hello { .. } => {}
                }
            }
        });
        Game {
            game: Arc::new(game),
        }
    }

    /// Fills the ring once the game was asked to, as the engine does: a numbered picture every
    /// 16 ms until the test ends.
    fn paint(&self) {
        let game = self.game.clone();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            let open = loop {
                if let Some(open) = game.share_engine().opened {
                    break open;
                }
                if Instant::now() > deadline {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            };
            let memory = Shm::open(&open.ring, open.ring_bytes as usize).unwrap();
            let mut writer =
                RingWriter::open(memory, open.width, open.height, open.ring_bytes as usize)
                    .unwrap();
            let mut number = 1u32;
            while Instant::now() < deadline && !game.share_engine().closed {
                let picture = TestPattern::render(open.width, open.height, number);
                writer.push_picture(&picture.bgra, u64::from(number) * 16_000, number);
                number += 1;
                std::thread::sleep(Duration::from_millis(16));
            }
        });
    }
}

async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{what}");
}

/// A host and a guest bound to each other, with the host's game painting.
async fn playing(mode: ShareMode) -> (support::relay::TestRelay, Node, Node, Game) {
    let relay = support::relay::start().await;
    let host = node(&relay).await;
    let guest = node(&relay).await;
    let game = Game::start(&host).await;
    game.paint();
    host.bind(Role::Host, &guest, mode);
    guest.bind(Role::Guest, &host, mode);
    eventually("the guest never saw pictures", || {
        guest.workers.shown.lock().unwrap().numbers.len() >= 10
            && guest.view().is_some_and(|v| v.phase == Phase::Live)
    })
    .await;
    (relay, host, guest, game)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_guest_sees_the_game_and_every_picture_is_whole() {
    let (_relay, host, guest, game) = playing(ShareMode::Watch).await;
    {
        let shown = guest.workers.shown.lock().unwrap();
        assert_eq!(shown.damaged, 0, "a damaged picture was shown");
        assert!(
            shown.numbers.windows(2).all(|w| w[0] < w[1]),
            "pictures out of order"
        );
    }
    // The host's worker made the ring and the game was asked for exactly that one.
    let launches = host.workers.launches.lock().unwrap().clone();
    assert_eq!(launches.len(), 1);
    let ring = launches[0].ring.clone().expect("a ring");
    let open = game
        .game
        .share_engine()
        .opened
        .expect("the game took the ring");
    assert_eq!(open.ring, ring.name);
    assert_eq!(open.mode, anyplay_bridge_protocol::ShareMode::Watch);
    // The guest's worker has no ring and reads the pad.
    let guest_launch = guest.workers.launches.lock().unwrap().clone();
    assert_eq!(guest_launch.len(), 1);
    assert_eq!(guest_launch[0].role, Role::Guest);
    assert_eq!(guest_launch[0].ring, None);
    // Both sides know the path and the mode, and the host told the cloud which path.
    eventually("the numbers", || {
        let (h, g) = (host.view(), guest.view());
        h.zip(g).is_some_and(|(h, g)| {
            h.link != Link::Connecting
                && g.link != Link::Connecting
                && g.stats != ShareStats::default()
                && g.codec.as_deref() == Some("test")
        })
    })
    .await;
    eventually("the path", || {
        host.saw(
            |s| matches!(s, Seen::Path(p) if matches!(p, SharePath::Direct | SharePath::Relay)),
        )
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hand_over_and_together_put_the_guests_pad_where_the_mode_says() {
    let (_relay, host, guest, game) = playing(ShareMode::HandOver).await;
    guest.workers.press(button::SOUTH);
    eventually("pad 1 to be the guest's", || {
        game.game.share_engine().now[0].is_some_and(|p| p.buttons == button::SOUTH)
    })
    .await;
    assert_eq!(game.game.share_engine().now[1], None);

    // The host moves the guest to pad 2; the guest hears it and keeps playing.
    host.hub().set_mode(ShareMode::Together);
    eventually("the guest to know the mode", || {
        guest.view().is_some_and(|v| v.mode == ShareMode::Together)
    })
    .await;
    eventually("pad 1 to be the host's again", || {
        game.game.share_engine().now[0].is_none()
    })
    .await;
    guest.workers.press(button::EAST);
    eventually("pad 2 to be the guest's", || {
        game.game.share_engine().now[1].is_some_and(|p| p.buttons == button::EAST)
    })
    .await;

    // Watch: the guest's pad is gone, and what the guest still sends reaches nobody.
    host.hub().set_mode(ShareMode::Watch);
    eventually("the guest pad to go", || {
        game.game.share_engine().now == [None, None]
    })
    .await;
    let taken = game.game.share_engine().taken.len();
    guest.workers.press(button::NORTH);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(game.game.share_engine().taken.len(), taken);
    assert_eq!(game.game.share_engine().refused, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_guest_who_vanishes_leaves_the_game_with_a_neutral_pad() {
    let (_relay, host, mut guest, game) = playing(ShareMode::HandOver).await;
    guest.workers.press(button::SOUTH | button::R1);
    eventually("the button", || {
        game.game.share_engine().now[0].is_some_and(|p| p.buttons != 0)
    })
    .await;
    // The guest's PC goes away in the middle of a held button.
    guest.hub = None;
    guest.tunnels.close().await;
    eventually("a neutral pad", || {
        game.game.share_engine().now[0].is_some_and(|p| p.buttons == 0)
    })
    .await;
    drop(host);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_guest_asks_and_the_host_hears() {
    let (_relay, host, guest, _game) = playing(ShareMode::Watch).await;
    guest.hub().request(ShareMode::Together);
    eventually("the host to hear the request", || {
        host.saw(|s| matches!(s, Seen::Request(ShareMode::Together)))
    })
    .await;
    // Asking changes nothing.
    assert_eq!(host.view().unwrap().mode, ShareMode::Watch);
    eventually("the host's view to show it", || {
        host.view()
            .is_some_and(|v| v.requested == Some(ShareMode::Together))
    })
    .await;
    // The host agrees: the request is answered and the guest follows.
    host.hub().set_mode(ShareMode::Together);
    eventually("the guest to follow", || {
        guest.view().is_some_and(|v| v.mode == ShareMode::Together)
    })
    .await;
    assert!(host.view().unwrap().requested.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_host_taking_control_back_makes_it_watch_for_everyone() {
    let (_relay, host, guest, game) = playing(ShareMode::Together).await;
    game.game.send(&Message::Override {
        action: OverrideAction::TookControl,
    });
    eventually("the host's service to report it", || {
        host.saw(|s| matches!(s, Seen::Mode(ShareMode::Watch)))
    })
    .await;
    eventually("the guest to be only watching", || {
        guest.view().is_some_and(|v| v.mode == ShareMode::Watch)
    })
    .await;
    assert_eq!(game.game.share_engine().now, [None, None]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_stop_key_ends_the_share_and_the_guest_is_told() {
    let (_relay, host, guest, game) = playing(ShareMode::HandOver).await;
    game.game.send(&Message::Override {
        action: OverrideAction::Stop,
    });
    eventually("the host's end", || {
        host.saw(|s| matches!(s, Seen::Ended(id, _) if id == "s1"))
    })
    .await;
    eventually("the guest's end", || {
        guest.saw(|s| matches!(s, Seen::Ended(id, _) if id == "s1"))
    })
    .await;
    eventually("the game to stop copying", || {
        game.game.share_engine().closed
    })
    .await;
    assert_eq!(game.game.share_engine().now, [None, None]);
    assert!(host.view().is_none() && guest.view().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_hosts_end_closes_the_game_and_the_guest_without_telling_the_cloud_twice() {
    let (_relay, host, guest, game) = playing(ShareMode::Watch).await;
    // The cloud ended it (a block, say): the hub closes quietly and does not report back.
    host.hub().unbind(play::code::BLOCKED, "blocked");
    eventually("the game to stop copying", || {
        game.game.share_engine().closed
    })
    .await;
    eventually("the guest to be told", || {
        guest.saw(|s| matches!(s, Seen::Ended(_, reason) if reason == "blocked"))
    })
    .await;
    assert!(!host.saw(|s| matches!(s, Seen::Ended(..))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn someone_who_is_not_the_guest_is_refused_and_starts_nothing() {
    let relay = support::relay::start().await;
    let host = node(&relay).await;
    let guest = node(&relay).await;
    let stranger = node(&relay).await;
    let _game = Game::start(&host).await;
    host.bind(Role::Host, &guest, ShareMode::HandOver);

    // The stranger knows the host's key and relay, as any friend of the host's may.
    let address = EndpointAddr::new(host.id()).with_relay_url(host.tunnels.home_relay().unwrap());
    let connection = stranger
        .tunnels
        .endpoint()
        .connect(address, play::ALPN)
        .await
        .expect("the handshake itself is open to everyone");
    let closed = tokio::time::timeout(Duration::from_secs(10), connection.closed())
        .await
        .expect("the host never hung up on the stranger");
    assert!(
        matches!(&closed, ConnectionError::ApplicationClosed(c) if u64::from(c.error_code) == 1),
        "{closed:?}"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        host.workers.launches.lock().unwrap().is_empty(),
        "a worker started for a stranger"
    );
    assert!(host.view().is_some_and(|v| v.phase == Phase::Connecting));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_a_share_nobody_connects() {
    let relay = support::relay::start().await;
    let host = node(&relay).await;
    let other = node(&relay).await;
    let address = EndpointAddr::new(host.id()).with_relay_url(host.tunnels.home_relay().unwrap());
    let connection = other
        .tunnels
        .endpoint()
        .connect(address, play::ALPN)
        .await
        .expect("the handshake itself is open to everyone");
    let closed = tokio::time::timeout(Duration::from_secs(10), connection.closed())
        .await
        .expect("the host never hung up");
    assert!(
        matches!(&closed, ConnectionError::ApplicationClosed(c) if u64::from(c.error_code) == 1),
        "{closed:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_game_that_cannot_open_the_ring_ends_the_share() {
    let relay = support::relay::start().await;
    let host = node(&relay).await;
    let guest = node(&relay).await;
    let game = Game::start(&host).await;
    game.game
        .refuse_share_open(anyplay_bridge_protocol::ShareResult::NotPresenting);
    host.bind(Role::Host, &guest, ShareMode::Watch);
    guest.bind(Role::Guest, &host, ShareMode::Watch);
    eventually("the share to end", || {
        host.saw(|s| matches!(s, Seen::Ended(_, reason) if reason == "no_game"))
    })
    .await;
    eventually("the guest to be told", || {
        guest.saw(|s| matches!(s, Seen::Ended(..)))
    })
    .await;
    assert!(
        host.saw(|s| matches!(s, Seen::Notice(message) if message.contains("would not share")))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loss_steps_the_picture_down_the_ladder_and_the_guest_follows() {
    let (_relay, host, guest, _game) = playing(ShareMode::Watch).await;
    // The game fills a ring of the ladder's top rung, whatever the stream starts at.
    let ring = host.workers.launches.lock().unwrap()[0]
        .ring
        .clone()
        .unwrap();
    assert_eq!((ring.width, ring.height, ring.fps), (1920, 1080, 60));
    let first = host.workers.setups(Role::Host)[0].clone();
    assert_eq!(first.epoch, 0);
    assert!(first.height >= 720, "the stream starts high: {first:?}");
    let shown_before = guest.workers.shown.lock().unwrap().numbers.len();

    // The guest reports heavy loss for a few seconds.
    for _ in 0..50 {
        guest.workers.report(50, 8);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    eventually("the host's worker to be given the smaller picture", || {
        host.workers
            .setups(Role::Host)
            .last()
            .is_some_and(|s| s.height == 540 && s.epoch > 0)
    })
    .await;
    let host_setup = host.workers.setups(Role::Host).last().cloned().unwrap();
    assert_eq!((host_setup.width, host_setup.fps), (960, 30));

    // The guest hears it in a config and gives its viewer the same epoch and size.
    eventually("the guest's viewer to follow", || {
        guest
            .workers
            .setups(Role::Guest)
            .last()
            .is_some_and(|s| (s.epoch, s.width, s.height) == (host_setup.epoch, 960, 540))
    })
    .await;

    // And the pictures go on, whole.
    eventually("pictures after the step", || {
        guest.workers.shown.lock().unwrap().numbers.len() > shown_before + 20
    })
    .await;
    let shown = guest.workers.shown.lock().unwrap();
    assert_eq!(shown.damaged, 0, "a damaged picture was shown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn both_sides_see_how_old_the_pictures_are_and_how_long_the_pad_takes() {
    let (_relay, host, guest, _game) = playing(ShareMode::HandOver).await;
    // The guest keeps pressing; the host acknowledges the states the game got.
    eventually("the figures on both sides", || {
        guest.workers.press(button::SOUTH);
        let figures = |node: &Node| {
            node.view()
                .map(|v| (v.stats.stream_delay_ms, v.stats.input_delay_ms))
        };
        matches!(figures(&guest), Some((Some(_), Some(_))))
            && matches!(figures(&host), Some((Some(_), Some(_))))
    })
    .await;
    // Both PCs are one machine here, so the figures are small; a clock that was not put right
    // would show seconds.
    for node in [&guest, &host] {
        let stats = node.view().unwrap().stats;
        let (stream, input) = (
            stats.stream_delay_ms.unwrap(),
            stats.input_delay_ms.unwrap(),
        );
        assert!(stream.p50 <= stream.p95 && stream.p95 < 1_000, "{stream:?}");
        assert!(input.p50 <= input.p95 && input.p95 < 1_000, "{input:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_hosts_limits_hold_the_picture_under_them() {
    let (_relay, host, guest, _game) = playing(ShareMode::Watch).await;
    host.hub().quality(Some(6_000), Some(540));
    // The ladder is at 720p30 or 720p60 at first; the limit takes it to 540p.
    for _ in 0..10 {
        guest.workers.report(50, 0);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    eventually("the picture to be 540p", || {
        host.workers
            .setups(Role::Host)
            .last()
            .is_some_and(|s| s.height == 540)
    })
    .await;
    let last = host.workers.setups(Role::Host).last().cloned().unwrap();
    assert!(last.kbps <= 6_000, "{last:?}");
}
