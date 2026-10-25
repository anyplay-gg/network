//! A stream between two net services with the real share worker program on each side: the
//! share link on loopback sockets, the token check, the child processes, and the test pattern
//! through the test codec and real iroh connections.

mod support;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyplay_control::{ScreenPhase, ScreenSource};
use anyplay_net::screen::{Hub, Member, Members, Output, View, Workers};
use anyplay_net::traffic::Traffic;
use anyplay_net::tunnel::{Options, RelayAccess, Tunnels};
use anyplay_net::worker_process::ProcessWorkers;
use iroh::SecretKey;
use tokio::sync::mpsc;

const WORKER: &str = env!("CARGO_BIN_EXE_anyplay-share");

struct Node {
    account: u64,
    tunnels: Tunnels,
    hub: Option<Hub>,
    views: Arc<Mutex<Vec<View>>>,
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
        views: Arc::new(Mutex::new(Vec::new())),
    }
}

impl Node {
    fn as_member(&self) -> (iroh::EndpointId, Member) {
        (
            self.tunnels.id(),
            Member {
                account: self.account,
                relay: self.tunnels.home_relay(),
            },
        )
    }

    fn join(&mut self, others: &[&Node]) {
        let workers = ProcessWorkers::new();
        let capabilities = workers.set_path(WORKER).expect("the worker answers");
        assert_eq!(capabilities.encode[0].codec, "test");
        let members: Members = others.iter().map(|n| n.as_member()).collect();
        let (out, mut rx) = mpsc::unbounded_channel();
        let views = self.views.clone();
        tokio::spawn(async move {
            while let Some(output) = rx.recv().await {
                if let Output::View(view) = output {
                    views.lock().unwrap().push(view);
                }
            }
        });
        let workers: Arc<dyn Workers> = Arc::new(workers);
        self.hub = Some(Hub::start(&self.tunnels, workers, members, out));
    }

    fn view(&self) -> View {
        self.views
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default()
    }
}

async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_worker_programs_carry_a_stream() {
    let relay = support::relay::start().await;
    let mut a = node(&relay, 1).await;
    let mut b = node(&relay, 2).await;
    a.join(&[&b]);
    b.join(&[&a]);

    let hub = a.hub.as_ref().unwrap();
    hub.start_sharing("Astro".into(), ScreenSource::TestPattern)
        .await
        .expect("start");
    eventually("no offer", || !b.view().offers.is_empty()).await;
    b.hub.as_ref().unwrap().watch(1).await.expect("watch");

    // The viewer's worker reports pictures shown, at the size of the test pattern.
    eventually("the viewer never showed a picture", || {
        b.view().watching.is_some_and(|w| {
            w.phase == ScreenPhase::Live && w.stats.width == 320 && w.stats.fps > 0
        })
    })
    .await;
    // The streamer's worker reports what it sends.
    eventually("the streamer never reported", || {
        a.view().hosting.is_some_and(|h| {
            h.phase == ScreenPhase::Live && h.stats.width == 320 && h.stats.kbps > 0
        })
    })
    .await;

    // Leaving ends the viewer's worker; stopping ends the streamer's.
    b.hub.as_ref().unwrap().leave();
    eventually("b did not leave", || b.view().watching.is_none()).await;
    a.hub.as_ref().unwrap().stop_sharing("ended");
    eventually("a did not stop", || a.view().hosting.is_none()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_path_that_is_no_worker_is_refused() {
    let workers = ProcessWorkers::new();
    assert!(workers.set_path("/definitely/not/here").is_err());
    let not_a_worker = std::env::current_exe().unwrap();
    // The test program itself answers --capabilities with nonsense (or a test list), not JSON.
    assert!(workers.set_path(&not_a_worker.to_string_lossy()).is_err());
    assert!(workers.capabilities().is_none());
}
