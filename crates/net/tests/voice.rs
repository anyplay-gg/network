//! Party voice between net services: real iroh endpoints and a relay on loopback, the engine on
//! memory devices, so no sound hardware is needed.
//!
//! Each node here is what the net service builds when a player in a party switches voice on: the
//! tunnels' endpoint, an engine on a memory device, and a hub that connects it to the members.

mod support;

use std::collections::HashMap;
use std::f32::consts::TAU;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyplay_control::{Link, VoiceState};
use anyplay_net::traffic::Traffic;
use anyplay_net::tunnel::{Options, RelayAccess, Tunnels};
use anyplay_net::voice::{self, Hub, Member, Members, Report, Settings};
use anyplay_voice::{Config, Engine, MemoryDevice, TalkMode};
use iroh::endpoint::ConnectionError;
use iroh::{EndpointAddr, EndpointId, SecretKey};
use tokio::sync::mpsc;

/// What a node's speakers did and what its hub reported.
#[derive(Default)]
struct Observed {
    /// When the speakers last played something audible.
    last_loud: Option<Instant>,
    latest: Option<Report>,
    links: Vec<Link>,
    states: Vec<VoiceState>,
}

struct Node {
    account: u64,
    tunnels: Tunnels,
    hub: Option<Hub>,
    engine: Option<Engine>,
    talking: Arc<AtomicBool>,
    observed: Arc<Mutex<Observed>>,
    stop: Arc<AtomicBool>,
    reports: Option<mpsc::UnboundedSender<Report>>,
}

impl Drop for Node {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

async fn node(relay: &support::relay::TestRelay, relay_only: bool, account: u64) -> Node {
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
            relay_only,
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
    let (device, speakers) = MemoryDevice::new(48_000, 48_000);
    let config = Config {
        noise_suppression: false,
        ..Config::default()
    };
    let engine = Engine::new(config, Box::new(device)).unwrap();

    let observed = Arc::new(Mutex::new(Observed::default()));
    let talking = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    // The hardware: every 20 ms the microphone hears a tone while the node talks, and the
    // speakers play what the engine queued.
    {
        let (observed, talking, stop) = (observed.clone(), talking.clone(), stop.clone());
        let hardware = speakers.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(20));
            let mut n = 0usize;
            while !stop.load(Ordering::Relaxed) {
                tick.tick().await;
                if talking.load(Ordering::Relaxed) {
                    let tone: Vec<f32> = (n..n + 960)
                        .map(|i| 0.3 * (TAU * 440.0 * i as f32 / 48_000.0).sin())
                        .collect();
                    n += 960;
                    hardware.feed_capture(&tone);
                }
                let played = hardware.play(960);
                let rms = (played.iter().map(|s| s * s).sum::<f32>() / 960.0).sqrt();
                if rms > 0.02 {
                    observed.lock().unwrap().last_loud = Some(Instant::now());
                }
            }
        });
    }
    let (reports, mut rx) = mpsc::unbounded_channel::<Report>();
    {
        let observed = observed.clone();
        tokio::spawn(async move {
            while let Some(report) = rx.recv().await {
                let mut seen = observed.lock().unwrap();
                seen.links
                    .extend(report.peers.iter().filter_map(|p| p.link));
                if seen.states.last() != Some(&report.state) {
                    seen.states.push(report.state);
                }
                seen.latest = Some(report);
            }
        });
    }
    Node {
        account,
        tunnels,
        hub: None,
        engine: Some(engine),
        talking,
        observed,
        stop,
        reports: Some(reports),
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
                voice: self.hub.is_some() || self.engine.is_some(),
            },
        )
    }

    /// Joins voice in a party with `others`.
    fn join(&mut self, others: &[&Node]) {
        let members: Members = others.iter().map(|n| n.as_member()).collect();
        let settings = Settings {
            mode: TalkMode::PushToTalk,
            talking: self.talking.load(Ordering::Relaxed),
            ..Settings::default()
        };
        self.hub = Some(Hub::start(
            &self.tunnels,
            self.engine.take().unwrap(),
            settings,
            members,
            self.reports.clone().unwrap(),
        ));
    }

    /// The player holds push-to-talk (and the microphone hears the tone), or lets go.
    fn talk(&self, on: bool) {
        self.talking.store(on, Ordering::Relaxed);
        if let Some(hub) = &self.hub {
            hub.set_talking(on);
        }
    }

    fn heard_within(&self, within: Duration) -> bool {
        self.observed
            .lock()
            .unwrap()
            .last_loud
            .is_some_and(|at| at.elapsed() < within)
    }

    fn report(&self) -> Option<Report> {
        self.observed.lock().unwrap().latest.clone()
    }

    fn link_to(&self, other: &Node) -> Option<Link> {
        self.report()?
            .peers
            .iter()
            .find(|p| p.account == other.account)
            .and_then(|p| p.link)
    }
}

async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{what}");
}

const QUIET: Duration = Duration::from_millis(400);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_members_hear_each_other() {
    let relay = support::relay::start().await;
    let mut a = node(&relay, false, 1).await;
    let mut b = node(&relay, false, 2).await;
    a.join(&[&b]);
    b.join(&[&a]);
    a.talk(true);
    b.talk(true);

    eventually("b never heard a", || b.heard_within(QUIET)).await;
    eventually("a never heard b", || a.heard_within(QUIET)).await;
    eventually("no voice link", || {
        a.link_to(&b).is_some() && b.link_to(&a).is_some()
    })
    .await;
    eventually("state never reached on", || {
        a.report().is_some_and(|r| r.state == VoiceState::On)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn in_a_party_of_three_a_talker_is_heard_by_both_others_and_not_by_themselves() {
    let relay = support::relay::start().await;
    let mut a = node(&relay, false, 1).await;
    let mut b = node(&relay, false, 2).await;
    let mut c = node(&relay, false, 3).await;
    a.join(&[&b, &c]);
    b.join(&[&a, &c]);
    c.join(&[&a, &b]);
    b.talk(true);

    eventually("a never heard b", || a.heard_within(QUIET)).await;
    eventually("c never heard b", || c.heard_within(QUIET)).await;
    eventually("a and c are not connected", || {
        a.link_to(&c).is_some() && c.link_to(&a).is_some()
    })
    .await;
    assert!(!b.heard_within(Duration::from_secs(60)), "b heard itself");

    // The one who was silent speaks next, and the other two hear them.
    b.talk(false);
    c.talk(true);
    let before = Instant::now();
    eventually("b never heard c", || {
        b.observed
            .lock()
            .unwrap()
            .last_loud
            .is_some_and(|at| at > before)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn someone_who_is_not_a_member_is_refused() {
    let relay = support::relay::start().await;
    let mut a = node(&relay, false, 1).await;
    let mut b = node(&relay, false, 2).await;
    let stranger = node(&relay, false, 9).await;
    a.join(&[&b]);
    b.join(&[&a]);

    // The stranger knows a's key and relay, as any friend of a's may, and dials the voice ALPN.
    let address = EndpointAddr::new(a.id()).with_relay_url(a.tunnels.home_relay().unwrap());
    let connection = stranger
        .tunnels
        .endpoint()
        .connect(address, voice::ALPN)
        .await
        .expect("the handshake itself is open to everyone");
    let closed = tokio::time::timeout(Duration::from_secs(10), connection.closed())
        .await
        .expect("a never hung up on the stranger");
    assert!(
        matches!(&closed, ConnectionError::ApplicationClosed(c) if u64::from(c.error_code) == 1),
        "{closed:?}"
    );

    // Whatever the stranger sends, it is not played, and a's party is still just b.
    a.talk(false);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let report = a.report().expect("report");
    assert_eq!(report.peers.len(), 1);
    assert_eq!(report.peers[0].account, 2);
    b.talk(true);
    eventually("the member was not heard", || a.heard_within(QUIET)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_under_another_key_is_a_stranger_too() {
    let relay = support::relay::start().await;
    let mut a = node(&relay, false, 1).await;
    let b = node(&relay, false, 2).await;
    // b's account is a member of a's party, but under the key of b's other device.
    let other_device = node(&relay, false, 2).await;
    a.join(&[&b]);

    let address = EndpointAddr::new(a.id()).with_relay_url(a.tunnels.home_relay().unwrap());
    let connection = other_device
        .tunnels
        .endpoint()
        .connect(address, voice::ALPN)
        .await
        .unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(10), connection.closed())
        .await
        .expect("never refused");
    assert!(
        matches!(&closed, ConnectionError::ApplicationClosed(c) if u64::from(c.error_code) == 1),
        "{closed:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leaving_the_party_stops_the_stream() {
    let relay = support::relay::start().await;
    let mut a = node(&relay, false, 1).await;
    let mut b = node(&relay, false, 2).await;
    let mut c = node(&relay, false, 3).await;
    a.join(&[&b, &c]);
    b.join(&[&a, &c]);
    c.join(&[&a, &b]);
    a.talk(true);
    eventually("b never heard a", || b.heard_within(QUIET)).await;
    eventually("c never heard a", || c.heard_within(QUIET)).await;
    eventually("links", || {
        b.link_to(&a).is_some() && b.link_to(&c).is_some()
    })
    .await;

    // c is removed from a's and b's member list, as when the party loses c: its voice is
    // dropped on both sides, a's stream goes on to b only.
    let only = |n: &Node| -> Members { HashMap::from([n.as_member()]) };
    a.hub.as_ref().unwrap().set_members(only(&b));
    b.hub.as_ref().unwrap().set_members(only(&a));
    eventually("c still has a link to a", || c.link_to(&a).is_none()).await;
    eventually("a still has a link to c", || a.link_to(&c).is_none()).await;
    let silent_since = Instant::now();
    eventually("c still hears a", || {
        !c.heard_within(QUIET) && silent_since.elapsed() > QUIET
    })
    .await;
    assert!(b.heard_within(QUIET), "b lost a with c");
    assert_eq!(
        a.report()
            .unwrap()
            .peers
            .iter()
            .map(|p| p.account)
            .collect::<Vec<_>>(),
        vec![2]
    );

    // a leaves altogether: dropping the hub hangs up, b goes quiet and sees the link gone.
    a.hub = None;
    eventually("b still has a link to a", || b.link_to(&a).is_none()).await;
    eventually("b still hears a", || !b.heard_within(QUIET)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_only_keeps_voice_on_the_relay() {
    let relay = support::relay::start().await;
    let mut a = node(&relay, true, 1).await;
    let mut b = node(&relay, true, 2).await;
    let mut c = node(&relay, true, 3).await;
    a.join(&[&b, &c]);
    b.join(&[&a, &c]);
    c.join(&[&a, &b]);
    a.talk(true);

    eventually("b never heard a", || b.heard_within(QUIET)).await;
    eventually("c never heard a", || c.heard_within(QUIET)).await;
    eventually("not every link is up", || {
        a.link_to(&b) == Some(Link::Relay)
            && a.link_to(&c) == Some(Link::Relay)
            && b.link_to(&c) == Some(Link::Relay)
    })
    .await;
    for node in [&a, &b, &c] {
        let seen = node.observed.lock().unwrap();
        assert!(
            seen.links
                .iter()
                .all(|l| matches!(l, Link::Relay | Link::Connecting)),
            "a voice connection went direct: {:?}",
            seen.links
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_with_voice_off_is_not_dialed_and_leaves_nothing_to_wait_for() {
    let relay = support::relay::start().await;
    let mut a = node(&relay, false, 1).await;
    // b is in the party but has voice off: a does not dial, and a has nobody to wait for.
    let b = node(&relay, false, 2).await;
    let (id, mut member) = b.as_member();
    member.voice = false;
    let engine = a.engine.take().unwrap();
    let hub = Hub::start(
        &a.tunnels,
        engine,
        Settings::default(),
        HashMap::from([(id, member)]),
        a.reports.clone().unwrap(),
    );
    eventually("a is not on", || {
        a.report().is_some_and(|r| r.state == VoiceState::On)
    })
    .await;
    assert_eq!(a.link_to(&b), None);
    drop(hub);
}
