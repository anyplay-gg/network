//! Party voice on the tunnels' iroh endpoint.
//!
//! One [`Hub`] is the voice side of one party on this PC. It owns the [`anyplay_voice::Engine`]
//! (and so the sound devices) and one QUIC connection to each other member who takes part, with
//! the ALPN [`ALPN`] on the same endpoint as the game tunnels, so relay-only, hole punching and
//! tickets apply to it as they do there (`spec/voice.md`, ADR 0010).
//!
//! Who may connect is the party's member list and nothing else. A connection counts only when its
//! remote endpoint, which the TLS handshake proves, is the endpoint the cloud named for a current
//! member; everything else is closed at once. Of each pair the smaller endpoint dials and the
//! other accepts, as for tunnels, and a dialer that is refused or loses its connection tries again
//! with a growing pause.
//!
//! The hub is one task: it pumps the engine every 5 ms, sends what the engine produced to every
//! connection, and gives each member's datagrams to the engine under that member's account. The
//! service tells it about members, push-to-talk, volumes and mutes, and takes its [`Report`]s.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyplay_control::{Link, VoiceState};
use anyplay_voice::{AudioIo, Engine, Level, PeerId, TalkMode};
use bytes::Bytes;
use futures_util::StreamExt;
use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayUrl};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::tunnel::Tunnels;

/// The protocol of a voice connection.
pub const ALPN: &[u8] = b"anyplay/voice/0";

/// What takes the connections that arrive with [`ALPN`].
pub type Sink = Arc<dyn Fn(Connection) + Send + Sync>;

/// How often the engine is pumped.
const PUMP: Duration = Duration::from_millis(5);
/// How often a report may leave: ten a second.
const REPORT: Duration = Duration::from_millis(100);
/// Datagrams from members waiting for the hub. When the hub is behind, the newest are dropped:
/// voice that late is no use.
const INBOX: usize = 512;
/// How long a dial waits for an answer.
const DIAL_TIMEOUT: Duration = Duration::from_secs(20);
const FIRST_PAUSE: Duration = Duration::from_millis(500);
const LONGEST_PAUSE: Duration = Duration::from_secs(30);
/// A connection that held this long was a good one, so its dialer starts over with a short pause.
const GOOD_CONNECTION: Duration = Duration::from_secs(10);

/// Application close codes of a voice connection.
const NOT_IN_PARTY: u32 = 1;
const LEFT_PARTY: u32 = 2;
const REPLACED: u32 = 3;
const ENDED: u32 = 0;

// Sound devices

/// Where the engine's microphone and speakers come from. The net service uses the system's
/// ([`Audio::system`]); tests use memory devices.
pub trait AudioBackend: Send + Sync + 'static {
    /// The names of the microphones, in the system's order.
    fn input_names(&self) -> Vec<String>;
    /// The names of the speakers, in the system's order.
    fn output_names(&self) -> Vec<String>;
    /// Opens a microphone and speakers by name; `None` is the default. May block.
    fn open(&self, input: Option<&str>, output: Option<&str>) -> Result<Box<dyn AudioIo>, String>;
}

/// A shareable [`AudioBackend`].
#[derive(Clone)]
pub struct Audio(Arc<dyn AudioBackend>);

impl Audio {
    pub fn new(backend: impl AudioBackend) -> Audio {
        Audio(Arc::new(backend))
    }

    /// The system's microphones and speakers when this build has them (feature `devices`),
    /// else a backend that has none and fails to open.
    pub fn system() -> Audio {
        Audio::new(System)
    }

    pub fn input_names(&self) -> Vec<String> {
        self.0.input_names()
    }

    pub fn output_names(&self) -> Vec<String> {
        self.0.output_names()
    }

    pub fn open(
        &self,
        input: Option<&str>,
        output: Option<&str>,
    ) -> Result<Box<dyn AudioIo>, String> {
        self.0.open(input, output)
    }
}

impl fmt::Debug for Audio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Audio")
    }
}

struct System;

#[cfg(feature = "devices")]
impl AudioBackend for System {
    fn input_names(&self) -> Vec<String> {
        anyplay_voice::input_names()
    }

    fn output_names(&self) -> Vec<String> {
        anyplay_voice::output_names()
    }

    fn open(&self, input: Option<&str>, output: Option<&str>) -> Result<Box<dyn AudioIo>, String> {
        anyplay_voice::CpalDevice::open_named(input, output)
            .map(|device| Box::new(device) as Box<dyn AudioIo>)
            .map_err(|e| e.to_string())
    }
}

#[cfg(not(feature = "devices"))]
impl AudioBackend for System {
    fn input_names(&self) -> Vec<String> {
        Vec::new()
    }

    fn output_names(&self) -> Vec<String> {
        Vec::new()
    }

    fn open(&self, _: Option<&str>, _: Option<&str>) -> Result<Box<dyn AudioIo>, String> {
        Err("this build of the net service has no sound devices".into())
    }
}

// The hub

/// A member of the party other than this player.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub account: u64,
    pub relay: Option<RelayUrl>,
    /// The member has voice switched on, so this side dials them (when its endpoint is the
    /// smaller). Members without it are still accepted from: they may be about to dial.
    pub voice: bool,
}

/// The members voice may connect to, by the endpoint the cloud named for each.
pub type Members = HashMap<EndpointId, Member>;

/// What the player set, which a new hub starts with.
#[derive(Clone, Debug, Default)]
pub struct Settings {
    pub mode: TalkMode,
    pub talking: bool,
    /// Volume per account, 0.0 to 2.0; missing is 1.0.
    pub volumes: HashMap<u64, f32>,
    pub muted: HashSet<u64>,
    /// The player's own microphone is muted.
    pub mic_muted: bool,
    /// The player hears nobody (and sends nothing).
    pub deafened: bool,
}

/// One other member in a [`Report`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PeerReport {
    pub account: u64,
    pub level: Level,
    /// The voice connection to this member and the path it takes; none when there is none.
    pub link: Option<Link>,
}

/// What the hub sees, at most ten times a second and only when it changed.
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    pub state: VoiceState,
    pub me: Level,
    pub peers: Vec<PeerReport>,
}

enum Ctl {
    Stop,
    Accepted(Connection),
    Members(Members),
    Talking(bool),
    Mode(TalkMode),
    Volume(u64, f32),
    Muted(u64, bool),
    MicMuted(bool),
    Deafened(bool),
}

enum Net {
    Dialed(EndpointId, Connection),
    Datagram(EndpointId, usize, Bytes),
    Path(EndpointId, usize, Link),
    Closed(EndpointId, usize),
}

/// The voice of one party. Dropping it hangs up: the connections close, the devices are released.
pub struct Hub {
    ctl: mpsc::UnboundedSender<Ctl>,
    tunnels: Tunnels,
}

impl Hub {
    /// Starts voice on `tunnels`' endpoint. Needs a Tokio runtime.
    pub fn start(
        tunnels: &Tunnels,
        engine: Engine,
        settings: Settings,
        members: Members,
        reports: mpsc::UnboundedSender<Report>,
    ) -> Hub {
        let (ctl_tx, ctl) = mpsc::unbounded_channel();
        let (net_tx, net) = mpsc::channel(INBOX);
        let sink_ctl = ctl_tx.clone();
        tunnels.set_voice_sink(Some(Arc::new(move |connection| {
            let _ = sink_ctl.send(Ctl::Accepted(connection));
        })));
        let task = Task {
            endpoint: tunnels.endpoint().clone(),
            me: tunnels.id(),
            engine,
            settings,
            members: HashMap::new(),
            live: HashMap::new(),
            dialers: HashMap::new(),
            net: net_tx,
            reports,
            last: None,
        };
        let mut task = task;
        task.set_members(members);
        tokio::spawn(task.run(ctl, net));
        Hub {
            ctl: ctl_tx,
            tunnels: tunnels.clone(),
        }
    }

    /// The party's members changed. Connections to those no longer in it close.
    pub fn set_members(&self, members: Members) {
        let _ = self.ctl.send(Ctl::Members(members));
    }

    pub fn set_talking(&self, talking: bool) {
        let _ = self.ctl.send(Ctl::Talking(talking));
    }

    pub fn set_mode(&self, mode: TalkMode) {
        let _ = self.ctl.send(Ctl::Mode(mode));
    }

    /// 0.0 (silent) to 2.0.
    pub fn set_volume(&self, account: u64, volume: f32) {
        let _ = self.ctl.send(Ctl::Volume(account, volume));
    }

    pub fn set_mic_muted(&self, muted: bool) {
        let _ = self.ctl.send(Ctl::MicMuted(muted));
    }

    pub fn set_deafened(&self, deafened: bool) {
        let _ = self.ctl.send(Ctl::Deafened(deafened));
    }

    pub fn set_muted(&self, account: u64, muted: bool) {
        let _ = self.ctl.send(Ctl::Muted(account, muted));
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.tunnels.set_voice_sink(None);
        let _ = self.ctl.send(Ctl::Stop);
    }
}

/// Aborts its task when dropped.
struct Guard(JoinHandle<()>);

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Live {
    connection: Connection,
    stable: usize,
    account: u64,
    link: Link,
    _tasks: [Guard; 2],
}

struct Dialer {
    relay: Option<RelayUrl>,
    _task: Guard,
}

struct Task {
    endpoint: Endpoint,
    me: EndpointId,
    engine: Engine,
    settings: Settings,
    members: Members,
    live: HashMap<EndpointId, Live>,
    dialers: HashMap<EndpointId, Dialer>,
    net: mpsc::Sender<Net>,
    reports: mpsc::UnboundedSender<Report>,
    last: Option<Report>,
}

impl Task {
    async fn run(mut self, mut ctl: mpsc::UnboundedReceiver<Ctl>, mut net: mpsc::Receiver<Net>) {
        self.engine.set_mode(self.settings.mode);
        self.engine.set_talking(self.settings.talking);
        self.engine.set_mic_muted(self.settings.mic_muted);
        self.engine.set_deafened(self.settings.deafened);
        let mut pump = tokio::time::interval(PUMP);
        pump.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut report = tokio::time::interval(REPORT);
        report.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = pump.tick() => self.pump(),
                _ = report.tick() => self.report(),
                message = ctl.recv() => match message {
                    Some(Ctl::Stop) | None => break,
                    Some(message) => self.control(message),
                },
                Some(message) = net.recv() => self.network(message),
            }
        }
        for live in self.live.values() {
            live.connection.close(ENDED.into(), b"voice ended");
        }
        // Dropping the engine releases the devices; dropping the guards ends the tasks.
    }

    fn pump(&mut self) {
        self.engine.pump();
        for datagram in self.engine.outgoing() {
            let datagram = Bytes::from(datagram);
            for live in self.live.values() {
                // A datagram that does not fit or a connection that just closed is a lost frame.
                let _ = live.connection.send_datagram(datagram.clone());
            }
        }
    }

    fn control(&mut self, message: Ctl) {
        match message {
            Ctl::Stop => {}
            Ctl::Accepted(connection) => self.accept(connection),
            Ctl::Members(members) => self.set_members(members),
            Ctl::Talking(talking) => {
                self.settings.talking = talking;
                self.engine.set_talking(talking);
            }
            Ctl::Mode(mode) => {
                self.settings.mode = mode;
                self.engine.set_mode(mode);
            }
            Ctl::MicMuted(muted) => {
                self.settings.mic_muted = muted;
                self.engine.set_mic_muted(muted);
            }
            Ctl::Deafened(deafened) => {
                self.settings.deafened = deafened;
                self.engine.set_deafened(deafened);
            }
            Ctl::Volume(account, volume) => {
                self.settings.volumes.insert(account, volume);
                self.engine.set_peer_volume(PeerId(account), volume);
            }
            Ctl::Muted(account, muted) => {
                if muted {
                    self.settings.muted.insert(account);
                } else {
                    self.settings.muted.remove(&account);
                }
                self.engine.mute(PeerId(account), muted);
            }
        }
    }

    fn network(&mut self, message: Net) {
        match message {
            Net::Dialed(id, connection) => self.attach(id, connection),
            Net::Datagram(id, stable, bytes) => {
                if let Some(live) = self.live.get(&id).filter(|l| l.stable == stable) {
                    // A datagram that is not a voice frame is dropped; the engine says why.
                    if let Err(reason) = self.engine.incoming(PeerId(live.account), &bytes) {
                        tracing::debug!("voice datagram refused: {reason}");
                    }
                }
            }
            Net::Path(id, stable, link) => {
                if let Some(live) = self.live.get_mut(&id).filter(|l| l.stable == stable) {
                    live.link = link;
                }
            }
            Net::Closed(id, stable) => {
                if self.live.get(&id).is_some_and(|l| l.stable == stable) {
                    self.live.remove(&id);
                }
            }
        }
    }

    /// A connection arrived: it stays only when it is a current member's, from the larger side of
    /// the pair.
    fn accept(&mut self, connection: Connection) {
        let id = connection.remote_id();
        if !self.members.contains_key(&id) || id >= self.me {
            tracing::debug!("voice: refused {}", id.fmt_short());
            connection.close(NOT_IN_PARTY.into(), b"not in the party");
            return;
        }
        self.attach(id, connection);
    }

    /// Keeps `connection` as the voice connection to member `id`.
    fn attach(&mut self, id: EndpointId, connection: Connection) {
        let Some(account) = self.members.get(&id).map(|m| m.account) else {
            connection.close(NOT_IN_PARTY.into(), b"not in the party");
            return;
        };
        if let Some(old) = self.live.remove(&id) {
            old.connection.close(REPLACED.into(), b"replaced");
        }
        let stable = connection.stable_id();
        let reader = {
            let connection = connection.clone();
            let net = self.net.clone();
            Guard(tokio::spawn(async move {
                while let Ok(datagram) = connection.read_datagram().await {
                    // Behind: drop the newest rather than queue voice nobody can use.
                    let _ = net.try_send(Net::Datagram(id, stable, datagram));
                }
                let _ = net.send(Net::Closed(id, stable)).await;
            }))
        };
        let paths = {
            let connection = connection.clone();
            let net = self.net.clone();
            Guard(tokio::spawn(async move {
                let mut stream = connection.paths_stream();
                while let Some(paths) = stream.next().await {
                    let link = match paths.iter().find(|p| p.is_selected()) {
                        Some(path) if path.is_relay() => Link::Relay,
                        Some(_) => Link::Direct,
                        None => Link::Connecting,
                    };
                    if net.send(Net::Path(id, stable, link)).await.is_err() {
                        return;
                    }
                }
            }))
        };
        self.live.insert(
            id,
            Live {
                connection,
                stable,
                account,
                link: Link::Connecting,
                _tasks: [reader, paths],
            },
        );
    }

    fn set_members(&mut self, members: Members) {
        let before = std::mem::replace(&mut self.members, members);
        self.members.remove(&self.me);
        let members = &self.members;
        self.live.retain(|id, live| {
            let stays = members.get(id).is_some_and(|m| m.account == live.account);
            if !stays {
                live.connection.close(LEFT_PARTY.into(), b"left the party");
            }
            stays
        });
        for member in before.values() {
            if !members.values().any(|m| m.account == member.account) {
                self.engine.remove_peer(PeerId(member.account));
            }
        }
        for member in members.values() {
            if before.values().any(|m| m.account == member.account) {
                continue;
            }
            let account = PeerId(member.account);
            if let Some(volume) = self.settings.volumes.get(&member.account) {
                self.engine.set_peer_volume(account, *volume);
            }
            if self.settings.muted.contains(&member.account) {
                self.engine.mute(account, true);
            }
        }
        self.dialers.retain(|id, dialer| {
            members
                .get(id)
                .is_some_and(|m| m.voice && m.relay == dialer.relay)
        });
        for (id, member) in members {
            if member.voice && self.me < *id && !self.dialers.contains_key(id) {
                let task = tokio::spawn(dial(
                    self.endpoint.clone(),
                    *id,
                    member.relay.clone(),
                    self.net.clone(),
                ));
                self.dialers.insert(
                    *id,
                    Dialer {
                        relay: member.relay.clone(),
                        _task: Guard(task),
                    },
                );
            }
        }
    }

    fn report(&mut self) {
        let levels = self.engine.levels();
        let quantized = |level: Level| Level {
            rms: (level.rms * 100.0).round() / 100.0,
            speaking: level.speaking,
        };
        let mut peers: Vec<PeerReport> = self
            .members
            .iter()
            .map(|(id, member)| PeerReport {
                account: member.account,
                level: levels
                    .peers
                    .iter()
                    .find(|(peer, _)| *peer == PeerId(member.account))
                    .map(|(_, level)| quantized(*level))
                    .unwrap_or_default(),
                link: self.live.get(id).map(|l| l.link),
            })
            .collect();
        peers.sort_by_key(|p| p.account);
        let waiting = self.members.values().any(|m| m.voice) && self.live.is_empty();
        let report = Report {
            state: if waiting {
                VoiceState::Connecting
            } else {
                VoiceState::On
            },
            me: quantized(levels.me),
            peers,
        };
        if self.last.as_ref() != Some(&report) {
            self.last = Some(report.clone());
            let _ = self.reports.send(report);
        }
    }
}

/// Dials member `id` until the task is aborted: after each end of the connection, and after each
/// failure, it waits a little longer than before.
async fn dial(endpoint: Endpoint, id: EndpointId, relay: Option<RelayUrl>, net: mpsc::Sender<Net>) {
    let mut pause = FIRST_PAUSE;
    loop {
        let mut addr = EndpointAddr::new(id);
        if let Some(relay) = &relay {
            addr = addr.with_relay_url(relay.clone());
        }
        match tokio::time::timeout(DIAL_TIMEOUT, endpoint.connect(addr, ALPN)).await {
            Ok(Ok(connection)) => {
                let since = Instant::now();
                if net.send(Net::Dialed(id, connection.clone())).await.is_err() {
                    return;
                }
                connection.closed().await;
                if since.elapsed() >= GOOD_CONNECTION {
                    pause = FIRST_PAUSE;
                }
            }
            Ok(Err(error)) => tracing::debug!("voice dial {}: {error}", id.fmt_short()),
            Err(_) => tracing::debug!("voice dial {}: no answer", id.fmt_short()),
        }
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(LONGEST_PAUSE);
    }
}
