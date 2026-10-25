//! A stream between two net services with the real share worker program on each side: the
//! share link on loopback sockets, the token check, the child processes, and the test pattern
//! through the test codec and real iroh connections.

mod support;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyplay_control::{ScreenPhase, ScreenSource};
#[cfg(windows)]
use anyplay_net::screen::GameWindow;
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

    fn join(&mut self, others: &[&Node], headless: bool) {
        let workers = if headless {
            ProcessWorkers::new().headless()
        } else {
            ProcessWorkers::new()
        };
        let capabilities = workers.set_path(WORKER).expect("the worker answers");
        // The test codec is always there; on Windows H.264 from the system comes before it.
        assert!(capabilities.encode.iter().any(|c| c.codec == "test"));
        assert!(capabilities.decode.iter().any(|c| c.codec == "test"));
        // On Linux the worker says how its OpenH264 library stands, whether or not it is there.
        if cfg!(target_os = "linux") {
            let names: Vec<_> = capabilities.libraries.iter().map(|l| &l.name).collect();
            assert_eq!(names, ["openh264"]);
        }
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
    a.join(&[&b], true);
    b.join(&[&a], true);

    let hub = a.hub.as_ref().unwrap();
    hub.start_sharing("Astro".into(), ScreenSource::TestPattern)
        .await
        .expect("start");
    eventually("no offer", || !b.view().offers.is_empty()).await;
    b.hub.as_ref().unwrap().watch(1).await.expect("watch");

    // The viewer's worker reports pictures shown (the test codec keeps the test pattern's size,
    // H.264 from the system has the size of the stream).
    eventually("the viewer never showed a picture", || {
        b.view().watching.is_some_and(|w| {
            w.phase == ScreenPhase::Live && w.stats.width >= 320 && w.stats.fps > 0
        })
    })
    .await;
    // The streamer's worker reports what it sends.
    eventually("the streamer never reported", || {
        a.view().hosting.is_some_and(|h| {
            h.phase == ScreenPhase::Live && h.stats.width >= 320 && h.stats.kbps > 0
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

/// The whole way on a desktop: a window of this process is captured by the streamer's worker
/// (found by this process's id), encoded by the system, cut into datagrams, carried over real
/// iroh connections through a relay, decoded by the viewer's worker and drawn in its window.
/// Ignored by default because it opens two windows. `HOLD_SECONDS` keeps them up for a look.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "opens windows: needs a desktop that is shown"]
async fn a_window_is_captured_streamed_and_shown() {
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = stop.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let mut window = minifb::Window::new(
            "AnyPlay test window (this is what is streamed)",
            640,
            360,
            minifb::WindowOptions::default(),
        )
        .expect("a window");
        window.set_target_fps(0);
        let _ = ready_tx.send(());
        let mut tick = 0u32;
        while !flag.load(std::sync::atomic::Ordering::Relaxed) && window.is_open() {
            // A moving picture: a blue field with a bar that runs across.
            tick = tick.wrapping_add(1);
            let bar = (tick as usize * 4) % 640;
            let buffer: Vec<u32> = (0..640 * 360)
                .map(|i| {
                    let x: usize = i % 640;
                    if x.abs_diff(bar) < 24 {
                        0xf0f0f0
                    } else {
                        0x2060c0
                    }
                })
                .collect();
            window
                .update_with_buffer(&buffer, 640, 360)
                .expect("update");
            std::thread::sleep(Duration::from_millis(15));
        }
    });
    ready_rx.recv().expect("window");

    let relay = support::relay::start().await;
    let mut a = node(&relay, 1).await;
    let mut b = node(&relay, 2).await;
    a.join(&[&b], true);
    b.join(&[&a], false);

    a.hub
        .as_ref()
        .unwrap()
        .start_sharing_game(
            "Test window".into(),
            ScreenSource::Game,
            GameWindow {
                window: None,
                pids: vec![std::process::id()],
            },
        )
        .await
        .expect("start");
    eventually("no offer", || !b.view().offers.is_empty()).await;
    b.hub.as_ref().unwrap().watch(1).await.expect("watch");
    eventually("the viewer never showed a picture", || {
        b.view().watching.is_some_and(|w| {
            w.phase == ScreenPhase::Live && w.stats.width >= 640 && w.stats.fps > 5
        })
    })
    .await;
    let seconds: u64 = std::env::var("HOLD_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if seconds > 0 {
        println!("LIVE: the viewer window is up for {seconds} s");
    }
    tokio::time::sleep(Duration::from_secs(seconds)).await;
    let view = b.view().watching.expect("still watching");
    println!(
        "viewer: {}x{} at {} fps, {} kbit/s",
        view.stats.width, view.stats.height, view.stats.fps, view.stats.kbps
    );

    b.hub.as_ref().unwrap().leave();
    a.hub.as_ref().unwrap().stop_sharing("ended");
    eventually("a did not stop", || a.view().hosting.is_none()).await;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = thread.join();
}
