//! Share play on the tunnels' iroh endpoint (`spec/share-play.md`, ADR 0019).
//!
//! One [`Hub`] holds at most one share: the connection with the ALPN [`ALPN`] to the one party
//! member the cloud's `Share` names, the share worker, and on the host's PC the game's side
//! ([`ShareHost`]). The hub looks into neither pictures nor sound; the worker (behind
//! [`Workers`]) captures, encodes, reassembles, decodes and reads the guest's pad.
//!
//! Who may connect is the `Share` and nothing else: the hub is told the one endpoint (and relay)
//! of the other side, and a connection counts only when its remote endpoint, which the TLS
//! handshake proves, is that one. Of the pair the smaller endpoint ID dials. Pad datagrams reach
//! the game only from that connection, only while the mode has a pad, and only through the
//! rules of [`ShareHost`].
//!
//! The hub never talks to the cloud or the launcher. It is told what the cloud decided
//! ([`Hub::bind`], [`Hub::unbind`]) and reports what happened on this PC as [`Output`]s, which
//! the service turns into cloud requests, bridge frames and launcher events.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyplay_api::{ShareMode, SharePath};
use anyplay_bridge_protocol as game;
use anyplay_control::{Link, ShareStats};
use anyplay_share::control::{self as wire, Role};
use anyplay_share::link::{FromWorker, RingReady, Start, ToWorker, WorkerStats};
use anyplay_share::pad::{PadSender, TYPE_PAD};
use anyplay_share::rate::{PathKind, RateConfig, RateController, RateInput};
use bytes::Bytes;
use futures_util::StreamExt;
use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayUrl};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::screen::{MAX_QUEUED_MESSAGES, PeerTx, PlayLaunch, RingLaunch, Sink, Workers};
use crate::shareplay::{Event as HostEvent, ShareHost, Step};
use crate::tunnel::Tunnels;
use crate::worker_process::ToWorkerTx;

/// The protocol of a share connection.
pub const ALPN: &[u8] = b"anyplay/share/0";

/// Application close codes of a share connection (`spec/share-play.md`, The share connection).
pub mod code {
    pub const ENDED: u32 = 0;
    pub const NOT_THE_GUEST: u32 = 1;
    pub const HOST_STOPPED: u32 = 2;
    pub const GUEST_LEFT: u32 = 3;
    pub const REPLACED: u32 = 4;
    pub const BLOCKED: u32 = 5;
    pub const PROTOCOL: u32 = 6;
    pub const TOO_MANY_INVALID: u32 = 7;
    pub const TIMED_OUT: u32 = 8;
}

const TICK: Duration = Duration::from_millis(50);
/// The numbers of a view leave at most this often; a change of shape leaves at the next tick.
const NUMBERS_PACE: Duration = Duration::from_millis(500);
const PING_EVERY: Duration = Duration::from_secs(1);
/// A viewer that gets nothing for this long shows that it waits.
const STALL_AFTER: Duration = Duration::from_secs(5);
/// A share whose peer says nothing for this long, or that never connects, ends.
const GIVE_UP_AFTER: Duration = Duration::from_secs(60);
const DIAL_TIMEOUT: Duration = Duration::from_secs(20);
const FIRST_PAUSE: Duration = Duration::from_millis(500);
const LONGEST_PAUSE: Duration = Duration::from_secs(30);
const GOOD_CONNECTION: Duration = Duration::from_secs(10);
/// How long a closing connection is given to deliver its last control message.
const CLOSE_GRACE: Duration = Duration::from_millis(200);
/// Control messages the guest may send per second.
const MESSAGES_PER_SECOND: u32 = 50;
const DEFAULT_PAYLOAD: usize = 1180;

/// The one share this PC takes part in, as the cloud named it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bind {
    pub id: String,
    /// Which side this PC is.
    pub role: Role,
    /// The other side's tunnel key: the only endpoint that may connect.
    pub peer: EndpointId,
    pub relay: Option<RelayUrl>,
    /// The game's title, for the viewer's window.
    pub title: String,
    pub mode: ShareMode,
}

/// What the launcher shows about the share, without who the other side is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    pub phase: Phase,
    pub mode: ShareMode,
    pub link: Link,
    pub codec: Option<String>,
    pub stats: ShareStats,
    /// The guest asked for this mode and the host has not answered (host only).
    pub requested: Option<ShareMode>,
}

/// How far the share has come on this PC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Connecting,
    Live,
    Stalled,
}

impl View {
    /// The view without the numbers that change all the time.
    fn shape(&self) -> View {
        let mut shape = self.clone();
        shape.stats = ShareStats::default();
        shape
    }
}

/// What the hub tells the service.
#[derive(Debug)]
pub enum Output {
    /// The share's view; none once there is no share.
    View(Option<View>),
    /// A frame for the game's bridge (host).
    Bridge(game::Frame),
    /// The host took control back on their own pad: the mode is `watch` now. The cloud's copy
    /// follows.
    Mode(ShareMode),
    /// The guest asks for another mode (host).
    Request(ShareMode),
    /// Which path the connection takes (host), for the cloud's quota rule.
    Path(SharePath),
    /// The share `id` ended on this PC's side, for this reason; the cloud is told. The view
    /// follows.
    Ended { id: String, reason: String },
    /// Something failed that no command waits for.
    Notice { code: &'static str, message: String },
}

enum Ctl {
    Stop,
    Accepted(Connection),
    Bind(Bind),
    Unbind { code: u32, reason: String },
    End { code: u32, reason: String },
    Mode(ShareMode),
    Request(ShareMode),
    Bridge(game::Frame),
    BridgeLost,
    Refresh,
}

enum Net {
    Dialed(EndpointId, Connection),
    Datagram(usize, Bytes),
    Control(usize, wire::Message),
    ControlBroken(usize),
    Path(usize, Link),
    Closed(usize),
    Worker(u64, FromWorker),
}

/// The share side of one PC. Dropping it hangs up: the connection closes, the worker ends.
pub struct Hub {
    ctl: mpsc::UnboundedSender<Ctl>,
    tunnels: Tunnels,
}

impl Hub {
    /// Starts on `tunnels`' endpoint. Needs a Tokio runtime.
    pub fn start(
        tunnels: &Tunnels,
        workers: Arc<dyn Workers>,
        out: mpsc::UnboundedSender<Output>,
    ) -> Hub {
        let (ctl_tx, ctl) = mpsc::unbounded_channel();
        let (net_tx, net) = mpsc::channel(1024);
        let sink_ctl = ctl_tx.clone();
        let sink: Sink = Arc::new(move |connection| {
            let _ = sink_ctl.send(Ctl::Accepted(connection));
        });
        tunnels.set_play_sink(Some(sink));
        let task = Task {
            endpoint: tunnels.endpoint().clone(),
            me: tunnels.id(),
            workers,
            active: None,
            net: net_tx,
            out,
            last_view: None,
            last_numbers: Instant::now(),
            generation: 0,
            clock: Instant::now(),
            connections: 0,
        };
        tokio::spawn(task.run(ctl, net));
        Hub {
            ctl: ctl_tx,
            tunnels: tunnels.clone(),
        }
    }

    /// Holds this share from now on: dials the other side when this PC's key is the smaller,
    /// and takes only its connection otherwise. A share held already is replaced, except when
    /// it is this very one, which only follows a changed relay.
    pub fn bind(&self, bind: Bind) {
        let _ = self.ctl.send(Ctl::Bind(bind));
    }

    /// The cloud ended the share (or this PC is told so): everything closes with `code`, the
    /// peer hears `reason`, and nothing is reported back to the cloud.
    pub fn unbind(&self, code: u32, reason: &str) {
        let _ = self.ctl.send(Ctl::Unbind {
            code,
            reason: reason.to_owned(),
        });
    }

    /// This player ends the share (stop, leave, game over): like [`Hub::unbind`], and the
    /// service is told with [`Output::Ended`] so that the cloud follows.
    pub fn end(&self, code: u32, reason: &str) {
        let _ = self.ctl.send(Ctl::End {
            code,
            reason: reason.to_owned(),
        });
    }

    /// The host changes the mode.
    pub fn set_mode(&self, mode: ShareMode) {
        let _ = self.ctl.send(Ctl::Mode(mode));
    }

    /// The guest asks the host for a mode.
    pub fn request(&self, mode: ShareMode) {
        let _ = self.ctl.send(Ctl::Request(mode));
    }

    /// A share frame from the game's bridge (host).
    pub fn bridge(&self, frame: game::Frame) {
        let _ = self.ctl.send(Ctl::Bridge(frame));
    }

    /// The game's bridge went away (host).
    pub fn bridge_lost(&self) {
        let _ = self.ctl.send(Ctl::BridgeLost);
    }

    /// Reports the view again.
    pub fn refresh(&self) {
        let _ = self.ctl.send(Ctl::Refresh);
    }
}

impl Hub {
    /// A handle that carries the game's side of the bridge to the hub from another task.
    pub fn handle(&self) -> Handle {
        Handle {
            ctl: self.ctl.clone(),
        }
    }
}

/// What a task holds that forwards the game's bridge to the hub.
#[derive(Clone)]
pub struct Handle {
    ctl: mpsc::UnboundedSender<Ctl>,
}

impl Handle {
    /// A share frame from the game's bridge.
    pub fn bridge(&self, frame: game::Frame) {
        let _ = self.ctl.send(Ctl::Bridge(frame));
    }

    /// The game's bridge went away.
    pub fn bridge_lost(&self) {
        let _ = self.ctl.send(Ctl::BridgeLost);
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.tunnels.set_play_sink(None);
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

struct Peer {
    connection: Connection,
    /// Numbers this connection among those the hub saw, so that a message of one that has gone
    /// is dropped.
    serial: usize,
    link: Link,
    rtt_ms: Option<u32>,
    writer: PeerTx,
    hello: Option<wire::Hello>,
    last_ping: Instant,
    messages: (Instant, u32),
    _tasks: [Guard; 3],
}

struct Worker {
    generation: u64,
    to: ToWorkerTx,
    /// The setup the worker runs with.
    start: Option<Start>,
    stats: WorkerStats,
    live: bool,
    last_progress: Instant,
    _forward: Guard,
}

struct Active {
    bind: Bind,
    mode: ShareMode,
    peer: Option<Peer>,
    dialer: Option<Guard>,
    /// The last time the peer was heard from, or the share began.
    heard: Instant,
    requested: Option<ShareMode>,
    codec: Option<String>,
    worker: Option<Worker>,
    // The host's side
    host: Option<ShareHost>,
    rate: Option<RateController>,
    loss_percent: u8,
    path_told: Option<Link>,
    // The guest's side
    pad: PadSender,
}

struct Task {
    endpoint: Endpoint,
    me: EndpointId,
    workers: Arc<dyn Workers>,
    active: Option<Active>,
    net: mpsc::Sender<Net>,
    out: mpsc::UnboundedSender<Output>,
    last_view: Option<Option<View>>,
    last_numbers: Instant,
    /// Numbers the workers, so that a message of a worker that has gone is dropped.
    generation: u64,
    clock: Instant,
    /// Counts the connections attached.
    connections: usize,
}

fn bridge_mode(mode: ShareMode) -> game::ShareMode {
    match mode {
        ShareMode::Watch | ShareMode::Unknown => game::ShareMode::Watch,
        ShareMode::HandOver => game::ShareMode::HandOver,
        ShareMode::Together => game::ShareMode::Together,
    }
}

fn wire_mode(mode: ShareMode) -> wire::Mode {
    match mode {
        ShareMode::Watch | ShareMode::Unknown => wire::Mode::Watch,
        ShareMode::HandOver => wire::Mode::HandOver,
        ShareMode::Together => wire::Mode::Together,
    }
}

fn api_mode(mode: wire::Mode) -> ShareMode {
    match mode {
        wire::Mode::Watch => ShareMode::Watch,
        wire::Mode::HandOver => ShareMode::HandOver,
        wire::Mode::Together => ShareMode::Together,
    }
}

fn path_kind(link: Link) -> PathKind {
    match link {
        Link::Direct => PathKind::Direct,
        // Until the path is known, assume the narrower one.
        Link::Relay | Link::Connecting => PathKind::Relay,
    }
}

/// Whether a mode gives the guest a pad.
fn has_pad(mode: ShareMode) -> bool {
    matches!(mode, ShareMode::HandOver | ShareMode::Together)
}

impl Task {
    async fn run(mut self, mut ctl: mpsc::UnboundedReceiver<Ctl>, mut net: mpsc::Receiver<Net>) {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = tick.tick() => self.tick(),
                message = ctl.recv() => match message {
                    Some(Ctl::Stop) | None => break,
                    Some(message) => self.control(message),
                },
                Some(message) = net.recv() => self.network(message),
            }
        }
        self.finish(code::ENDED, "ended", false);
    }

    fn now_us(&self) -> u64 {
        self.clock.elapsed().as_micros() as u64
    }

    fn send(&self, output: Output) {
        let _ = self.out.send(output);
    }

    // Commands

    fn control(&mut self, message: Ctl) {
        match message {
            Ctl::Stop => {}
            Ctl::Accepted(connection) => self.accept(connection),
            Ctl::Bind(bind) => self.bind(bind),
            Ctl::Unbind { code, reason } => self.finish(code, &reason, false),
            Ctl::End { code, reason } => self.finish(code, &reason, true),
            Ctl::Mode(mode) => self.set_mode(mode),
            Ctl::Request(mode) => self.request(mode),
            Ctl::Bridge(frame) => self.game_frame(&frame),
            Ctl::BridgeLost => self.bridge_lost(),
            Ctl::Refresh => self.last_view = None,
        }
        self.report(true);
    }

    fn bind(&mut self, bind: Bind) {
        if let Some(active) = &mut self.active {
            if active.bind.id == bind.id && active.bind.peer == bind.peer {
                // The same share: a changed relay changes where this PC dials.
                if active.bind.relay != bind.relay {
                    active.bind.relay = bind.relay;
                    if active.peer.is_none() {
                        active.dialer = None;
                        self.sync_dialer();
                    }
                }
                return;
            }
            self.finish(code::REPLACED, "replaced", true);
        }
        let now = Instant::now();
        let mode = bind.mode;
        self.active = Some(Active {
            bind,
            mode,
            peer: None,
            dialer: None,
            heard: now,
            requested: None,
            codec: None,
            worker: None,
            host: None,
            rate: None,
            loss_percent: 0,
            path_told: None,
            pad: PadSender::new(),
        });
        self.sync_dialer();
    }

    /// Dials the peer when this PC's key is the smaller and nothing is connected.
    fn sync_dialer(&mut self) {
        let Some(active) = &mut self.active else {
            return;
        };
        if active.peer.is_some()
            || active.dialer.is_some()
            || self.me.as_bytes() >= active.bind.peer.as_bytes()
        {
            return;
        }
        let task = tokio::spawn(dial(
            self.endpoint.clone(),
            active.bind.peer,
            active.bind.relay.clone(),
            self.net.clone(),
        ));
        active.dialer = Some(Guard(task));
    }

    fn set_mode(&mut self, mode: ShareMode) {
        if mode == ShareMode::Unknown {
            return;
        }
        let Some(active) = &mut self.active else {
            return;
        };
        if active.bind.role != Role::Host || active.mode == mode {
            return;
        }
        active.requested = None;
        let steps = active
            .host
            .as_mut()
            .map(|host| host.set_mode(bridge_mode(mode)))
            .unwrap_or_default();
        active.mode = mode;
        self.apply(steps);
        self.send_config();
    }

    fn request(&mut self, mode: ShareMode) {
        let Some(active) = &mut self.active else {
            return;
        };
        if active.bind.role != Role::Guest || mode == ShareMode::Unknown {
            return;
        }
        if let Some(peer) = &active.peer {
            let _ = peer.writer.send(wire::Message::Request(wire::Request {
                mode: wire_mode(mode),
            }));
        }
    }

    /// Ends the share: the game stops copying, the peer hears why, the connection closes with
    /// `close_code`, the worker ends. With `tell_service` the service is told, so that the
    /// cloud follows.
    fn finish(&mut self, close_code: u32, reason: &str, tell_service: bool) {
        let Some(mut active) = self.active.take() else {
            return;
        };
        if let Some(host) = &mut active.host {
            let steps = host.close();
            self.apply_steps_to_game(steps);
        }
        if let Some(worker) = &active.worker {
            let _ = worker.to.send(ToWorker::Stop);
        }
        if let Some(peer) = active.peer.take() {
            let _ = peer.writer.send(wire::Message::Stop(wire::Stop {
                reason: reason.to_owned(),
            }));
            // The peer's tasks live until the connection is closed, so that the last control
            // message is written first.
            tokio::spawn(async move {
                // The whole peer is held until the connection closes, tasks included: a block
                // that named only its connection would let the control stream's task go first.
                let peer = peer;
                tokio::time::sleep(CLOSE_GRACE).await;
                peer.connection.close(close_code.into(), b"share");
            });
        }
        if tell_service {
            self.send(Output::Ended {
                id: active.bind.id.clone(),
                reason: reason.to_owned(),
            });
        }
        self.last_view = None;
        self.report(true);
    }

    // Connections

    /// A connection arrived: it stays only when it is the share's other side.
    fn accept(&mut self, connection: Connection) {
        let id = connection.remote_id();
        if !self.active.as_ref().is_some_and(|a| a.bind.peer == id) {
            tracing::debug!("share: refused {}", id.fmt_short());
            connection.close(code::NOT_THE_GUEST.into(), b"not the guest");
            return;
        }
        self.attach(connection);
    }

    fn attach(&mut self, connection: Connection) {
        let Some(active) = &mut self.active else {
            connection.close(code::NOT_THE_GUEST.into(), b"no share");
            return;
        };
        if let Some(old) = active.peer.take() {
            old.connection.close(code::REPLACED.into(), b"replaced");
        }
        self.connections += 1;
        let serial = self.connections;
        let dialing = self.me.as_bytes() < active.bind.peer.as_bytes();
        // A peer that stops reading its control stream until this many messages pile up is
        // gone: the connection ends (code 8) rather than the queue growing or losing messages.
        let (writer, queue) = PeerTx::new(MAX_QUEUED_MESSAGES, {
            let connection = connection.clone();
            move || connection.close(code::TIMED_OUT.into(), b"control stream stalled")
        });
        // Our hello goes first on the stream.
        let caps = self.workers.capabilities().unwrap_or_default();
        let hello = match active.bind.role {
            Role::Host => wire::Hello {
                version: wire::VERSION,
                role: Role::Host,
                encode: caps.encode,
                decode: Vec::new(),
                title: active.bind.title.clone(),
            },
            Role::Guest => wire::Hello {
                version: wire::VERSION,
                role: Role::Guest,
                encode: Vec::new(),
                decode: caps.decode,
                title: String::new(),
            },
        };
        let _ = writer.send(wire::Message::Hello(hello));
        let control = Guard(tokio::spawn(control_stream(
            serial,
            dialing,
            connection.clone(),
            queue,
            self.net.clone(),
        )));
        let datagrams = {
            let connection = connection.clone();
            let net = self.net.clone();
            Guard(tokio::spawn(async move {
                while let Ok(datagram) = connection.read_datagram().await {
                    // Behind: drop the newest rather than queue video nobody can use.
                    let _ = net.try_send(Net::Datagram(serial, datagram));
                }
                let _ = net.send(Net::Closed(serial)).await;
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
                    if net.send(Net::Path(serial, link)).await.is_err() {
                        return;
                    }
                }
            }))
        };
        active.heard = Instant::now();
        active.peer = Some(Peer {
            connection,
            serial,
            link: Link::Connecting,
            rtt_ms: None,
            writer,
            hello: None,
            last_ping: Instant::now()
                .checked_sub(PING_EVERY)
                .unwrap_or_else(Instant::now),
            messages: (Instant::now(), 0),
            _tasks: [control, datagrams, paths],
        });
        active.dialer = None;
    }

    // Messages from the network and the workers

    fn network(&mut self, message: Net) {
        match message {
            Net::Dialed(id, connection) => {
                if self.active.as_ref().is_some_and(|a| a.bind.peer == id) {
                    self.attach(connection);
                } else {
                    connection.close(code::ENDED.into(), b"no share");
                }
            }
            Net::Datagram(serial, bytes) => {
                if self.current(serial) {
                    self.on_datagram(&bytes);
                }
            }
            Net::Control(serial, message) => {
                if self.current(serial) {
                    self.on_message(message);
                }
            }
            Net::ControlBroken(serial) => {
                if self.current(serial) {
                    self.finish(code::PROTOCOL, "protocol", true);
                }
            }
            Net::Path(serial, link) => {
                if let Some(active) = &mut self.active
                    && let Some(peer) = active.peer.as_mut().filter(|p| p.serial == serial)
                {
                    peer.link = link;
                    if let Some(rate) = &mut active.rate {
                        rate.set_path(path_kind(link));
                    }
                }
                self.tell_path();
            }
            Net::Closed(serial) => {
                if self.current(serial) {
                    // The connection is gone but the share is not: the dialing side dials again,
                    // and the share ends if nobody comes back (the give-up time).
                    if let Some(active) = &mut self.active {
                        active.peer = None;
                        active.heard = Instant::now();
                    }
                    self.sync_dialer();
                }
            }
            Net::Worker(generation, message) => self.on_worker(generation, message),
        }
        self.report(false);
    }

    fn current(&self, serial: usize) -> bool {
        self.active
            .as_ref()
            .and_then(|a| a.peer.as_ref())
            .is_some_and(|p| p.serial == serial)
    }

    /// The host tells the cloud which path the share takes, once it is known.
    fn tell_path(&mut self) {
        let Some(active) = &mut self.active else {
            return;
        };
        if active.bind.role != Role::Host {
            return;
        }
        let link = active.peer.as_ref().map(|p| p.link);
        if let Some(link @ (Link::Direct | Link::Relay)) = link
            && active.path_told != Some(link)
        {
            active.path_told = Some(link);
            let path = if link == Link::Relay {
                SharePath::Relay
            } else {
                SharePath::Direct
            };
            self.send(Output::Path(path));
        }
    }

    fn on_datagram(&mut self, bytes: &[u8]) {
        let now_us = self.now_us();
        let Some(active) = &mut self.active else {
            return;
        };
        active.heard = Instant::now();
        match active.bind.role {
            Role::Host => {
                // The guest sends pad states and nothing else is taken.
                if bytes.get(1) != Some(&TYPE_PAD) {
                    return;
                }
                let Some(host) = &mut active.host else {
                    return;
                };
                let steps = host.on_pad_datagram(now_us, bytes);
                self.apply(steps);
            }
            Role::Guest => {
                // The host sends video and sound; a pad datagram from it is nothing.
                if bytes.get(1) == Some(&TYPE_PAD) {
                    return;
                }
                if let Some(worker) = &active.worker {
                    let _ = worker.to.send(ToWorker::Datagram(bytes.to_vec()));
                }
            }
        }
    }

    fn on_message(&mut self, message: wire::Message) {
        let now = Instant::now();
        let now_us = self.now_us();
        let Some(active) = &mut self.active else {
            return;
        };
        active.heard = now;
        let role = active.bind.role;
        let Some(peer) = active.peer.as_mut() else {
            return;
        };
        if now.duration_since(peer.messages.0) >= Duration::from_secs(1) {
            peer.messages = (now, 0);
        }
        peer.messages.1 += 1;
        // Only the guest is limited: the host's messages are few and its own.
        if role == Role::Host && peer.messages.1 > MESSAGES_PER_SECOND {
            self.finish(code::PROTOCOL, "protocol", true);
            return;
        }
        match message {
            wire::Message::Hello(hello) => {
                let expected = match role {
                    Role::Host => Role::Guest,
                    Role::Guest => Role::Host,
                };
                if hello.version != wire::VERSION || hello.role != expected {
                    self.finish(code::PROTOCOL, "protocol", true);
                    return;
                }
                peer.hello = Some(hello);
                if role == Role::Host {
                    self.host_begin();
                }
            }
            wire::Message::Ping(ping) => {
                let _ = peer.writer.send(wire::Message::Pong(wire::Pong {
                    t0: ping.t0,
                    t1: now_us,
                    t2: now_us,
                }));
            }
            wire::Message::Pong(pong) => {
                let (rtt, _) = wire::clock_estimate(pong.t0, pong.t1, pong.t2, now_us);
                peer.rtt_ms = Some((rtt.max(0) / 1000) as u32);
            }
            wire::Message::Stop(stop) => {
                // The other side is ending it. This PC closes too and tells the cloud.
                tracing::info!("share: the peer stopped it: {}", stop.reason);
                let reason = if stop.reason.is_empty() {
                    "ended".to_owned()
                } else {
                    stop.reason
                };
                let code = match role {
                    Role::Host => code::GUEST_LEFT,
                    Role::Guest => code::HOST_STOPPED,
                };
                self.finish(code, &reason, true);
            }
            // The host's side
            wire::Message::Report(report) if role == Role::Host => self.guest_reports(report),
            wire::Message::Key(_) if role == Role::Host => {
                if let Some(worker) = &active.worker {
                    let _ = worker.to.send(ToWorker::KeyFrame);
                }
            }
            wire::Message::Request(request) if role == Role::Host => {
                let mode = api_mode(request.mode);
                active.requested = Some(mode);
                self.send(Output::Request(mode));
            }
            // The guest's side
            wire::Message::Config(config) if role == Role::Guest => self.guest_config(config),
            _ => {}
        }
    }

    // The host

    /// The guest's hello is in: launches the worker with the frame ring, or sends the setup
    /// again to a guest who came back.
    fn host_begin(&mut self) {
        let now_us = self.now_us();
        let payload = self.max_payload();
        let Some(active) = &mut self.active else {
            return;
        };
        if active.worker.is_some() {
            // A guest who dialed again: the setup and a key frame.
            if let Some(worker) = &active.worker {
                let _ = worker.to.send(ToWorker::KeyFrame);
            }
            self.send_config();
            return;
        }
        let caps = self.workers.capabilities().unwrap_or_default();
        let decodes = active
            .peer
            .as_ref()
            .and_then(|p| p.hello.as_ref())
            .map(|h| h.decode.clone())
            .unwrap_or_default();
        let Some(codec) = pick_codec(&caps.encode, &decodes) else {
            self.finish(code::PROTOCOL, "no_encoder", true);
            return;
        };
        let link = active.peer.as_ref().map_or(Link::Connecting, |p| p.link);
        let rate = RateController::new(RateConfig::default(), path_kind(link), now_us);
        let target = rate.target();
        let random: [u8; 16] = rand::random();
        let launch = PlayLaunch {
            role: Role::Host,
            title: active.bind.title.clone(),
            ring: Some(RingLaunch {
                name: anyplay_share::ring::name_from_random(random),
                width: target.width,
                height: target.height,
                fps: target.fps,
            }),
        };
        let port = match self.workers.launch_play(&launch) {
            Ok(port) => port,
            Err(error) => {
                tracing::warn!("share: host worker: {error}");
                self.send(Output::Notice {
                    code: "share_worker",
                    message: format!("Share play could not start: {error}"),
                });
                self.finish(code::HOST_STOPPED, "no_encoder", true);
                return;
            }
        };
        self.generation += 1;
        let generation = self.generation;
        let start = Start {
            epoch: 0,
            codec: codec.clone(),
            width: target.width,
            height: target.height,
            fps: target.fps,
            kbps: target.kbps,
            parity_percent: target.parity_percent,
            max_payload: payload,
        };
        let _ = port.to.send(ToWorker::Start(start.clone()));
        let forward = self.forward(generation, port.from);
        let Some(active) = &mut self.active else {
            return;
        };
        active.codec = Some(codec);
        active.rate = Some(rate);
        active.worker = Some(Worker {
            generation,
            to: port.to,
            start: Some(start),
            stats: WorkerStats::default(),
            live: false,
            last_progress: Instant::now(),
            _forward: forward,
        });
        self.send_config();
    }

    /// The worker made the ring: the game is asked to fill it.
    fn ring_ready(&mut self, ring: RingReady) {
        let now_us = self.now_us();
        let Some(active) = &mut self.active else {
            return;
        };
        if active.bind.role != Role::Host || active.host.is_some() {
            return;
        }
        let (host, steps) = ShareHost::open(now_us, &ring, bridge_mode(active.mode));
        active.host = Some(host);
        self.apply(steps);
    }

    /// Does what the host's side says: frames to the game, events acted on.
    fn apply(&mut self, steps: Vec<Step>) {
        for step in steps {
            match step {
                Step::Bridge(frame) => self.send(Output::Bridge(frame)),
                Step::Event(event) => self.host_event(event),
            }
        }
    }

    /// Like `apply` for steps that only carry frames (the share is being closed).
    fn apply_steps_to_game(&mut self, steps: Vec<Step>) {
        for step in steps {
            if let Step::Bridge(frame) = step {
                self.send(Output::Bridge(frame));
            }
        }
    }

    fn host_event(&mut self, event: HostEvent) {
        match event {
            HostEvent::Opened | HostEvent::Mode(_) | HostEvent::Status(_) => {}
            HostEvent::OpenFailed(result) => {
                let reason = match result {
                    game::ShareResult::Running => "busy",
                    game::ShareResult::NotPresenting => "no_game",
                    _ => "no_ring",
                };
                self.send(Output::Notice {
                    code: "share_game",
                    message: "The game would not share its picture.".into(),
                });
                self.finish(code::HOST_STOPPED, reason, true);
            }
            HostEvent::TookControl => {
                if let Some(active) = &mut self.active
                    && active.mode != ShareMode::Watch
                {
                    active.mode = ShareMode::Watch;
                    active.requested = None;
                    self.send_config();
                    self.send(Output::Mode(ShareMode::Watch));
                }
            }
            HostEvent::StopRequested => self.finish(code::HOST_STOPPED, "ended", true),
            HostEvent::CloseGuest(close_code) => {
                self.finish(u32::from(close_code), "protocol", true);
            }
            HostEvent::BridgeLost => self.finish(code::HOST_STOPPED, "game_ended", true),
        }
    }

    /// A frame from the game's bridge.
    fn game_frame(&mut self, frame: &game::Frame) {
        let Some(host) = self.active.as_mut().and_then(|a| a.host.as_mut()) else {
            return;
        };
        let steps = host.on_bridge(frame);
        self.apply(steps);
    }

    fn bridge_lost(&mut self) {
        let Some(host) = self.active.as_mut().and_then(|a| a.host.as_mut()) else {
            return;
        };
        let steps = host.bridge_lost();
        self.apply(steps);
    }

    /// The setup the guest needs: codec, size, rate and the mode.
    fn send_config(&mut self) {
        let Some(active) = &self.active else {
            return;
        };
        let (Some(peer), Some(start)) = (
            &active.peer,
            active.worker.as_ref().and_then(|w| w.start.as_ref()),
        ) else {
            return;
        };
        let _ = peer.writer.send(wire::Message::Config(wire::Config {
            epoch: start.epoch,
            codec: start.codec.clone(),
            width: start.width,
            height: start.height,
            fps: start.fps,
            kbps: start.kbps,
            mode: wire_mode(active.mode),
        }));
    }

    /// The datagram payload that fits the guest's connection.
    fn max_payload(&self) -> usize {
        self.active
            .as_ref()
            .and_then(|a| a.peer.as_ref())
            .and_then(|p| p.connection.max_datagram_size())
            .map(|size| size.saturating_sub(20))
            .unwrap_or(DEFAULT_PAYLOAD)
            .max(anyplay_share::packetiser::MIN_PAYLOAD)
    }

    /// The guest's report feeds the controller; its target goes to the worker. The picture
    /// keeps the size of the frame ring the game fills, so only the rate follows.
    fn guest_reports(&mut self, report: wire::Report) {
        let now_us = self.now_us();
        let Some(active) = &mut self.active else {
            return;
        };
        let total = u64::from(report.received) + u64::from(report.lost);
        active.loss_percent = (u64::from(report.lost) * 100)
            .checked_div(total)
            .map_or(0, |percent| percent.min(100)) as u8;
        let (Some(rate), Some(worker)) = (&mut active.rate, &mut active.worker) else {
            return;
        };
        let target = rate.on_input(&RateInput {
            now_us,
            received: report.received,
            lost: report.lost,
            delay_trend_us: report.delay_trend_us,
            send_queue_frames: 0,
            delivery_kbps: None,
        });
        let Some(start) = &mut worker.start else {
            return;
        };
        if (target.kbps, target.parity_percent) != (start.kbps, start.parity_percent) {
            start.kbps = target.kbps;
            start.parity_percent = target.parity_percent;
            let _ = worker.to.send(ToWorker::Rate {
                kbps: target.kbps,
                parity_percent: target.parity_percent,
            });
        }
    }

    // The guest

    /// The host's setup: starts the viewer's worker, or follows a new mode, or a new setup.
    fn guest_config(&mut self, config: wire::Config) {
        let Some(active) = &mut self.active else {
            return;
        };
        let mode = api_mode(config.mode);
        if active.mode != mode {
            active.mode = mode;
            active.requested = None;
        }
        let start = Start {
            epoch: config.epoch,
            codec: config.codec.clone(),
            width: config.width,
            height: config.height,
            fps: config.fps,
            kbps: config.kbps,
            parity_percent: 0,
            max_payload: 0,
        };
        active.codec = Some(config.codec);
        if let Some(worker) = &mut active.worker {
            // A change of mode alone does not restart the decoder.
            let same = worker.start.as_ref().is_some_and(|s| {
                (s.epoch, &s.codec, s.width, s.height, s.fps)
                    == (
                        start.epoch,
                        &start.codec,
                        start.width,
                        start.height,
                        start.fps,
                    )
            });
            if !same {
                let _ = worker.to.send(ToWorker::Start(start.clone()));
                worker.start = Some(start);
                if let Some(peer) = &active.peer {
                    let _ = peer.writer.send(wire::Message::Key(wire::Key {
                        epoch: config.epoch,
                        frame: 0,
                    }));
                }
            }
            return;
        }
        if self
            .workers
            .capabilities()
            .is_none_or(|c| c.decode.is_empty())
        {
            self.finish(code::GUEST_LEFT, "no_decoder", true);
            return;
        }
        let launch = PlayLaunch {
            role: Role::Guest,
            title: active.bind.title.clone(),
            ring: None,
        };
        let port = match self.workers.launch_play(&launch) {
            Ok(port) => port,
            Err(error) => {
                tracing::warn!("share: viewer worker: {error}");
                self.send(Output::Notice {
                    code: "share_worker",
                    message: format!("The share could not be shown: {error}"),
                });
                self.finish(code::GUEST_LEFT, "no_decoder", true);
                return;
            }
        };
        self.generation += 1;
        let generation = self.generation;
        let _ = port.to.send(ToWorker::Start(start.clone()));
        let forward = self.forward(generation, port.from);
        let Some(active) = &mut self.active else {
            return;
        };
        active.worker = Some(Worker {
            generation,
            to: port.to,
            start: Some(start),
            stats: WorkerStats::default(),
            live: false,
            last_progress: Instant::now(),
            _forward: forward,
        });
        // Whatever came before the setup was dropped: a key frame starts the picture.
        if let Some(peer) = &active.peer {
            let _ = peer.writer.send(wire::Message::Key(wire::Key {
                epoch: config.epoch,
                frame: 0,
            }));
        }
    }

    fn on_worker(&mut self, generation: u64, message: FromWorker) {
        let Some(active) = &mut self.active else {
            return;
        };
        let role = active.bind.role;
        let Some(worker) = active
            .worker
            .as_mut()
            .filter(|w| w.generation == generation)
        else {
            return;
        };
        match (role, message) {
            // A video or sound datagram of the host's worker goes to the guest.
            (Role::Host, FromWorker::Datagram(bytes)) => {
                if let Some(peer) = &active.peer {
                    // A datagram that does not fit or a connection that just closed is a lost
                    // packet.
                    let _ = peer.connection.send_datagram(Bytes::from(bytes));
                }
            }
            (Role::Host, FromWorker::RingReady(ring)) => self.ring_ready(ring),
            (_, FromWorker::Stats(stats)) => {
                if stats.frames > worker.stats.frames {
                    worker.last_progress = Instant::now();
                    worker.live = true;
                }
                worker.stats = stats;
            }
            (_, FromWorker::Ended(reason)) => {
                tracing::info!("share: worker ended: {reason}");
                self.send(Output::Notice {
                    code: "share_worker",
                    message: format!("Share play stopped: {reason}"),
                });
                let (code, reason) = match role {
                    Role::Host => (code::HOST_STOPPED, "ended"),
                    Role::Guest => (code::GUEST_LEFT, "ended"),
                };
                self.finish(code, reason, true);
            }
            // The guest's side
            (Role::Guest, FromWorker::Report(report)) => {
                if let Some(peer) = &active.peer {
                    let _ = peer.writer.send(wire::Message::Report(report));
                }
            }
            (Role::Guest, FromWorker::Key(request)) => {
                if let Some(peer) = &active.peer {
                    let _ = peer.writer.send(wire::Message::Key(request));
                }
            }
            (Role::Guest, FromWorker::Pad(state)) => {
                // A pad state goes out only while the mode has a pad; the host drops the rest
                // anyway.
                if has_pad(active.mode)
                    && let Some(peer) = &active.peer
                {
                    let datagram = active.pad.next(state);
                    let _ = peer.connection.send_datagram(Bytes::from(datagram));
                }
            }
            _ => {}
        }
    }

    /// Moves a worker's messages into the hub's inbox, tagged with who sent them.
    fn forward(&self, generation: u64, mut from: mpsc::Receiver<FromWorker>) -> Guard {
        let net = self.net.clone();
        Guard(tokio::spawn(async move {
            while let Some(message) = from.recv().await {
                if net.send(Net::Worker(generation, message)).await.is_err() {
                    return;
                }
            }
            let _ = net
                .send(Net::Worker(
                    generation,
                    FromWorker::Ended("the worker closed".into()),
                ))
                .await;
        }))
    }

    // Time

    fn tick(&mut self) {
        let now = Instant::now();
        let now_us = self.now_us();
        let clock = self.clock;
        let Some(active) = &mut self.active else {
            return;
        };
        if let Some(peer) = &mut active.peer
            && now.duration_since(peer.last_ping) >= PING_EVERY
        {
            peer.last_ping = now;
            let t0 = clock.elapsed().as_micros() as u64;
            let _ = peer.writer.send(wire::Message::Ping(wire::Ping { t0 }));
        }
        // Silence from the guest becomes a neutral pad in the game.
        if let Some(host) = &mut active.host {
            let steps = host.tick(now_us);
            self.apply(steps);
        }
        let Some(active) = &mut self.active else {
            return;
        };
        // Every second each side answers the other's ping, so a peer that is silent in every way
        // for a minute is gone, or never came.
        if now.duration_since(active.heard) >= GIVE_UP_AFTER {
            self.finish(code::TIMED_OUT, "timeout", true);
            return;
        }
        self.report(false);
    }

    // The view

    fn view(&self) -> Option<View> {
        let active = self.active.as_ref()?;
        let worker = active.worker.as_ref();
        let now = Instant::now();
        let phase = match worker {
            Some(w) if w.live && now.duration_since(w.last_progress) >= STALL_AFTER => {
                Phase::Stalled
            }
            Some(w) if w.live => Phase::Live,
            _ => Phase::Connecting,
        };
        let stats = worker.map(|w| w.stats).unwrap_or_default();
        Some(View {
            phase,
            mode: active.mode,
            link: active.peer.as_ref().map_or(Link::Connecting, |p| p.link),
            codec: active.codec.clone(),
            stats: ShareStats {
                width: stats.width,
                height: stats.height,
                fps: stats.fps,
                kbps: stats.kbps,
                rtt_ms: active.peer.as_ref().and_then(|p| p.rtt_ms),
                loss_percent: active.loss_percent,
            },
            requested: active.requested,
        })
    }

    /// Tells the service when the view changed: at the next opportunity for its shape, and
    /// twice a second for its numbers.
    fn report(&mut self, force: bool) {
        let view = self.view();
        if self.last_view.as_ref() == Some(&view) {
            return;
        }
        let shape_changed = self.last_view.as_ref().map(|v| v.as_ref().map(View::shape))
            != Some(view.as_ref().map(View::shape));
        let due = self.last_numbers.elapsed() >= NUMBERS_PACE;
        if shape_changed || due || force {
            self.last_numbers = Instant::now();
            self.last_view = Some(view.clone());
            self.send(Output::View(view));
        }
    }
}

/// AV1, then H.264, then whatever else both have, in the host's order. The test codec is
/// lossless and huge: only tests (which offer nothing else) get it.
fn pick_codec(encode: &[wire::CodecCaps], decode: &[wire::CodecCaps]) -> Option<String> {
    let both = |name: &str| {
        encode.iter().any(|c| c.codec == name) && decode.iter().any(|c| c.codec == name)
    };
    ["av1", "h264", "test"]
        .into_iter()
        .find(|name| both(name))
        .map(str::to_owned)
}

/// The control stream of one connection: the dialing side opens it, the other accepts it. Lines
/// from `queue` are written, lines read are parsed and handed to the hub.
async fn control_stream(
    serial: usize,
    dialing: bool,
    connection: Connection,
    mut queue: mpsc::Receiver<wire::Message>,
    net: mpsc::Sender<Net>,
) {
    let opened = if dialing {
        // We dialed, so we open the stream; the first line makes the other side see it.
        connection.open_bi().await
    } else {
        connection.accept_bi().await
    };
    let Ok((mut send, mut recv)) = opened else {
        return;
    };
    let reader_net = net.clone();
    let reader = tokio::spawn(async move {
        let mut buffer: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 2048];
        loop {
            match recv.read(&mut chunk).await {
                Ok(Some(n)) if n > 0 => buffer.extend_from_slice(&chunk[..n]),
                _ => return,
            }
            while let Some(end) = buffer.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buffer.drain(..=end).collect();
                match wire::parse_line(&line) {
                    Ok(Some(message)) => {
                        if reader_net
                            .send(Net::Control(serial, message))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Ok(None) => {}
                    Err(_) => {
                        let _ = reader_net.send(Net::ControlBroken(serial)).await;
                        return;
                    }
                }
            }
            if buffer.len() > wire::MAX_LINE {
                let _ = reader_net.send(Net::ControlBroken(serial)).await;
                return;
            }
        }
    });
    while let Some(message) = queue.recv().await {
        let Ok(line) = wire::encode_line(&message) else {
            continue;
        };
        if send.write_all(&line).await.is_err() {
            break;
        }
    }
    let _ = send.finish();
    reader.abort();
}

/// Dials `id` until the task is aborted: after each end of the connection, and after each
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
            Ok(Err(error)) => tracing::debug!("share dial {}: {error}", id.fmt_short()),
            Err(_) => tracing::debug!("share dial {}: timed out", id.fmt_short()),
        }
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(LONGEST_PAUSE);
    }
}
