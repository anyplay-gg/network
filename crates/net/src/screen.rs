//! Share screen on the tunnels' iroh endpoint (`spec/screen-share.md`, ADR 0023).
//!
//! One [`Hub`] is the screen side of this PC. It is a viewer all the time the player is in a
//! party: connections from members arrive with the ALPN [`ALPN`], and each is an offer to
//! watch. It is a streamer while the player shares: it dials the members and, once a member
//! says `watch`, starts the share worker and forwards the worker's datagrams to the members who
//! watch. The hub looks into neither pictures nor sound; the worker (behind [`Workers`]) does
//! the capturing, encoding, packetising, reassembling and decoding.
//!
//! Who may connect is the party's member list and nothing else: a connection counts only when
//! its remote endpoint, which the TLS handshake proves, is a current member's. The streamer
//! dials and the viewer accepts, so a pair that both stream holds two connections.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyplay_api::AccountId;
use anyplay_control::{
    Link, ScreenHosting, ScreenOffer, ScreenPhase, ScreenSource, ScreenStats, ScreenViewer,
    ScreenWatching,
};
use anyplay_share::control::{self as wire, CodecCaps, Message, Role};
use anyplay_share::link::{Capabilities, FromWorker, Start, ToWorker, WorkerStats};
use anyplay_share::rate::{PathKind, RateConfig, RateController, RateInput, Target};
use bytes::Bytes;
use futures_util::StreamExt;
use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayUrl};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::tunnel::Tunnels;
use crate::worker_process::ToWorkerTx;

/// The protocol of a screen connection.
pub const ALPN: &[u8] = b"anyplay/screen/0";

/// What takes the connections that arrive with [`ALPN`].
pub type Sink = Arc<dyn Fn(Connection) + Send + Sync>;

/// The most members that watch one stream at once.
pub const MAX_VIEWERS: usize = 4;

const TICK: Duration = Duration::from_millis(100);
/// The numbers of a view leave at most this often; a change of shape leaves at the next tick.
const NUMBERS_PACE: Duration = Duration::from_millis(500);
const PING_EVERY: Duration = Duration::from_secs(1);
/// A viewer that gets nothing for this long shows that it waits.
const STALL_AFTER: Duration = Duration::from_secs(5);
/// A viewer that waits this long leaves.
const GIVE_UP_AFTER: Duration = Duration::from_secs(60);
const DIAL_TIMEOUT: Duration = Duration::from_secs(20);
const FIRST_PAUSE: Duration = Duration::from_millis(500);
const LONGEST_PAUSE: Duration = Duration::from_secs(30);
const GOOD_CONNECTION: Duration = Duration::from_secs(10);
/// How long a closing connection is given to deliver its last control message.
const CLOSE_GRACE: Duration = Duration::from_millis(200);
/// Control messages a peer may send per second.
const MESSAGES_PER_SECOND: u32 = 50;
/// The payload a datagram carries when the connection does not say.
const DEFAULT_PAYLOAD: usize = 1180;

/// Application close codes of a screen connection.
pub mod code {
    pub const ENDED: u32 = 0;
    pub const NOT_IN_PARTY: u32 = 1;
    pub const STOPPED: u32 = 2;
    pub const REPLACED: u32 = 4;
    pub const LEFT_PARTY: u32 = 5;
    pub const PROTOCOL: u32 = 6;
    /// The other side stopped reading the control stream until [`super::MAX_QUEUED_MESSAGES`]
    /// messages piled up for it.
    pub const STALLED: u32 = 7;
}

/// Control messages waiting to be written to one peer. The hub answers and reports at a few
/// messages a second, so a peer that lets this many pile up has stopped reading its stream:
/// the connection is closed (code [`code::STALLED`]) rather than losing messages, or growing
/// the queue without limit.
pub const MAX_QUEUED_MESSAGES: usize = 256;

/// The hub's end of the queue of control messages to one peer: never blocks, bounded.
pub(crate) struct PeerTx {
    tx: mpsc::Sender<Message>,
    /// Ends the connection; called once when the queue overflows.
    on_overflow: Box<dyn Fn() + Send + Sync>,
}

impl PeerTx {
    pub(crate) fn new(
        capacity: usize,
        on_overflow: impl Fn() + Send + Sync + 'static,
    ) -> (PeerTx, mpsc::Receiver<Message>) {
        let (tx, rx) = mpsc::channel(capacity);
        (
            PeerTx {
                tx,
                on_overflow: Box::new(on_overflow),
            },
            rx,
        )
    }

    /// `true` when the message is queued. On overflow the connection is ended and the message
    /// is not queued; a closed queue (the connection is already gone) is not an overflow.
    pub(crate) fn send(&self, message: Message) -> bool {
        match self.tx.try_send(message) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                (self.on_overflow)();
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }
}

// The share worker, as the hub sees it

/// Where the game shows its picture, as far as the launcher knows (see `screen_start`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GameWindow {
    /// A Windows `HWND` as a number.
    pub window: Option<i64>,
    /// The processes of the game.
    pub pids: Vec<u32>,
}

/// The two ends of the conversation with one worker. Dropping `to` ends the worker.
pub struct WorkerPort {
    pub to: ToWorkerTx,
    pub from: mpsc::Receiver<FromWorker>,
}

/// How a worker is started for share play (`spec/share-play.md`): the host's reads the frame
/// ring the game fills, the guest's reads the guest's pad as well as showing the picture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayLaunch {
    pub role: Role,
    /// The game's title, for the viewer's window.
    pub title: String,
    /// The ring the host's worker makes (`--source ring`); none for the guest.
    pub ring: Option<RingLaunch>,
}

/// The frame ring a host's worker makes: its name and the picture it holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RingLaunch {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// Where workers come from: the child process of the real service, or memory in tests.
pub trait Workers: Send + Sync + 'static {
    /// What the worker can do; none while there is none.
    fn capabilities(&self) -> Option<Capabilities>;
    /// Starts a worker for one side. The first message it gets is [`ToWorker::Start`].
    fn launch(
        &self,
        role: Role,
        source: ScreenSource,
        title: &str,
        game: &GameWindow,
    ) -> Result<WorkerPort, String>;
    /// Starts a worker for share play. The first message it gets is [`ToWorker::Start`]; the
    /// host's worker says `RingReady` first. A set of workers that has no share play says so.
    fn launch_play(&self, _: &PlayLaunch) -> Result<WorkerPort, String> {
        Err("this share worker cannot do share play".into())
    }
    /// Tells it where the verified worker program is and answers what that can do. Only the
    /// real service has a program to point at.
    fn set_path(&self, _path: &str) -> Result<Capabilities, String> {
        Err("this build of the net service has no share worker".into())
    }
}

/// A shareable [`Workers`].
#[derive(Clone)]
pub struct WorkerSet(Arc<dyn Workers>);

impl WorkerSet {
    pub fn new(workers: impl Workers) -> WorkerSet {
        WorkerSet(Arc::new(workers))
    }

    /// A set without any worker: nothing can be shared or watched.
    pub fn none() -> WorkerSet {
        WorkerSet::new(NoWorkers)
    }

    pub fn capabilities(&self) -> Option<Capabilities> {
        self.0.capabilities()
    }

    pub fn set_path(&self, path: &str) -> Result<Capabilities, String> {
        self.0.set_path(path)
    }

    pub(crate) fn shared(&self) -> Arc<dyn Workers> {
        self.0.clone()
    }
}

impl std::fmt::Debug for WorkerSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WorkerSet")
    }
}

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
        Err("there is no share worker".into())
    }
}

// What the hub reports

/// What the launcher shows about share screen, without the two flags the service adds.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct View {
    pub hosting: Option<ScreenHosting>,
    pub offers: Vec<ScreenOffer>,
    pub watching: Option<ScreenWatching>,
}

impl View {
    /// The view without the numbers that change all the time.
    fn shape(&self) -> View {
        let mut shape = self.clone();
        if let Some(hosting) = &mut shape.hosting {
            hosting.stats = ScreenStats::default();
            for viewer in &mut hosting.viewers {
                viewer.rtt_ms = None;
                viewer.loss_percent = 0;
            }
        }
        if let Some(watching) = &mut shape.watching {
            watching.stats = ScreenStats::default();
            watching.rtt_ms = None;
            watching.shown_fps = 0;
        }
        shape
    }
}

/// What the hub sends to the service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Output {
    View(View),
    /// Something failed that no command waits for.
    Notice {
        code: &'static str,
        message: String,
    },
}

/// Why a command was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// No worker, or it cannot capture or encode.
    NoEncoder,
    /// No worker, or it cannot decode.
    NoDecoder,
    /// Already sharing, or already watching.
    Busy,
    /// That member offers no stream.
    NotFound,
}

/// A member of the party other than this player.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub account: u64,
    pub relay: Option<RelayUrl>,
}

/// The members a stream may reach, by the endpoint the cloud named for each.
pub type Members = HashMap<EndpointId, Member>;

// The hub

enum Ctl {
    Stop,
    Accepted(Connection),
    Members(Members),
    Start {
        title: String,
        source: ScreenSource,
        game: GameWindow,
        reply: oneshot::Sender<Result<(), Refusal>>,
    },
    StopSharing(&'static str),
    Watch {
        account: u64,
        reply: oneshot::Sender<Result<(), Refusal>>,
    },
    Leave,
    Refresh,
}

/// Which way a connection was made: `Out` is dialed by this PC, which then streams to that
/// member; `In` was accepted, and the member streams to this PC.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Side {
    Out,
    In,
}

type PeerKey = (EndpointId, Side);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkerRole {
    Streamer,
    Viewer,
}

enum Net {
    Dialed(EndpointId, Connection),
    Datagram(PeerKey, usize, Bytes),
    Control(PeerKey, usize, Message),
    ControlBroken(PeerKey, usize),
    Path(PeerKey, usize, Link),
    Closed(PeerKey, usize),
    Worker(WorkerRole, u64, FromWorker),
}

/// The screen side of one PC. Dropping it hangs up: connections close, workers end.
pub struct Hub {
    ctl: mpsc::UnboundedSender<Ctl>,
    tunnels: Tunnels,
}

impl Hub {
    /// Starts on `tunnels`' endpoint. Needs a Tokio runtime.
    pub fn start(
        tunnels: &Tunnels,
        workers: Arc<dyn Workers>,
        members: Members,
        out: mpsc::UnboundedSender<Output>,
    ) -> Hub {
        let (ctl_tx, ctl) = mpsc::unbounded_channel();
        let (net_tx, net) = mpsc::channel(1024);
        let sink_ctl = ctl_tx.clone();
        tunnels.set_screen_sink(Some(Arc::new(move |connection| {
            let _ = sink_ctl.send(Ctl::Accepted(connection));
        })));
        let mut task = Task {
            endpoint: tunnels.endpoint().clone(),
            me: tunnels.id(),
            workers,
            members: HashMap::new(),
            peers: HashMap::new(),
            dialers: HashMap::new(),
            hosting: None,
            watching: None,
            net: net_tx,
            out,
            last_view: None,
            last_numbers: Instant::now(),
            generation: 0,
            clock: Instant::now(),
        };
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

    /// Starts sharing: dials the members and offers them the stream of `title`.
    pub async fn start_sharing(&self, title: String, source: ScreenSource) -> Result<(), Refusal> {
        self.start_sharing_game(title, source, GameWindow::default())
            .await
    }

    /// Like `start_sharing`, with the game's window and processes for the share worker.
    pub async fn start_sharing_game(
        &self,
        title: String,
        source: ScreenSource,
        game: GameWindow,
    ) -> Result<(), Refusal> {
        let (reply, answer) = oneshot::channel();
        let _ = self.ctl.send(Ctl::Start {
            title,
            source,
            game,
            reply,
        });
        answer.await.unwrap_or(Err(Refusal::NoEncoder))
    }

    /// Ends the stream this PC shares, if any. `reason` is what the viewers are told.
    pub fn stop_sharing(&self, reason: &'static str) {
        let _ = self.ctl.send(Ctl::StopSharing(reason));
    }

    /// Watches the stream `account` offers.
    pub async fn watch(&self, account: u64) -> Result<(), Refusal> {
        let (reply, answer) = oneshot::channel();
        let _ = self.ctl.send(Ctl::Watch { account, reply });
        answer.await.unwrap_or(Err(Refusal::NotFound))
    }

    pub fn leave(&self) {
        let _ = self.ctl.send(Ctl::Leave);
    }

    /// Reports the view again, as after the worker changed.
    pub fn refresh(&self) {
        let _ = self.ctl.send(Ctl::Refresh);
    }
}

impl Hub {
    /// A handle for async commands that outlive a borrow of the hub.
    pub fn handle(&self) -> Handle {
        Handle {
            ctl: self.ctl.clone(),
        }
    }
}

/// What a command task holds while it waits for the hub's answer.
pub struct Handle {
    ctl: mpsc::UnboundedSender<Ctl>,
}

impl Handle {
    pub async fn start_sharing(&self, title: String, source: ScreenSource) -> Result<(), Refusal> {
        self.start_sharing_game(title, source, GameWindow::default())
            .await
    }

    /// Like `start_sharing`, with the game's window and processes for the share worker.
    pub async fn start_sharing_game(
        &self,
        title: String,
        source: ScreenSource,
        game: GameWindow,
    ) -> Result<(), Refusal> {
        let (reply, answer) = oneshot::channel();
        let _ = self.ctl.send(Ctl::Start {
            title,
            source,
            game,
            reply,
        });
        answer.await.unwrap_or(Err(Refusal::NoEncoder))
    }

    pub async fn watch(&self, account: u64) -> Result<(), Refusal> {
        let (reply, answer) = oneshot::channel();
        let _ = self.ctl.send(Ctl::Watch { account, reply });
        answer.await.unwrap_or(Err(Refusal::NotFound))
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.tunnels.set_screen_sink(None);
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
    stable: usize,
    account: u64,
    link: Link,
    rtt_ms: Option<u32>,
    writer: PeerTx,
    /// What the other side told in its `hello`.
    hello: Option<wire::Hello>,
    /// `Out`: the member chose to watch. `In`: this player chose to watch.
    watching: bool,
    /// `Out` while watching: the controller of the path to this member.
    rate: Option<RateController>,
    loss_percent: u8,
    last_ping: Instant,
    /// Control messages in the current second.
    messages: (Instant, u32),
    _tasks: [Guard; 3],
}

/// The stream this PC shares.
struct Hosting {
    title: String,
    source: ScreenSource,
    game: GameWindow,
    worker: Option<HostWorker>,
}

struct HostWorker {
    generation: u64,
    to: ToWorkerTx,
    start: Start,
    stats: WorkerStats,
    _forward: Guard,
}

/// The stream this PC watches.
struct Watching {
    key: PeerKey,
    generation: u64,
    to: ToWorkerTx,
    stats: WorkerStats,
    live: bool,
    last_progress: Instant,
    since: Instant,
    _forward: Guard,
}

struct Dialer {
    relay: Option<RelayUrl>,
    _task: Guard,
}

struct Task {
    endpoint: Endpoint,
    me: EndpointId,
    workers: Arc<dyn Workers>,
    members: Members,
    peers: HashMap<PeerKey, Peer>,
    dialers: HashMap<EndpointId, Dialer>,
    hosting: Option<Hosting>,
    watching: Option<Watching>,
    net: mpsc::Sender<Net>,
    out: mpsc::UnboundedSender<Output>,
    last_view: Option<View>,
    last_numbers: Instant,
    /// Numbers the workers, so that a message of a worker that has gone is dropped.
    generation: u64,
    clock: Instant,
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
        for peer in self.peers.values() {
            peer.connection.close(code::ENDED.into(), b"screen ended");
        }
        if let Some(watching) = &self.watching {
            let _ = watching.to.send(ToWorker::Stop);
        }
        if let Some(worker) = self.hosting.as_ref().and_then(|h| h.worker.as_ref()) {
            let _ = worker.to.send(ToWorker::Stop);
        }
    }

    fn now_us(&self) -> u64 {
        self.clock.elapsed().as_micros() as u64
    }

    // Commands

    fn control(&mut self, message: Ctl) {
        match message {
            Ctl::Stop => {}
            Ctl::Accepted(connection) => self.accept(connection),
            Ctl::Members(members) => self.set_members(members),
            Ctl::Start {
                title,
                source,
                game,
                reply,
            } => {
                let _ = reply.send(self.start_sharing(title, source, game));
            }
            Ctl::StopSharing(reason) => self.stop_sharing(reason),
            Ctl::Watch { account, reply } => {
                let _ = reply.send(self.watch(account));
            }
            Ctl::Leave => self.leave(),
            Ctl::Refresh => self.last_view = None,
        }
        self.report(true);
    }

    fn start_sharing(
        &mut self,
        title: String,
        source: ScreenSource,
        game: GameWindow,
    ) -> Result<(), Refusal> {
        if self.hosting.is_some() {
            return Err(Refusal::Busy);
        }
        let can = self
            .workers
            .capabilities()
            .is_some_and(|c| !c.encode.is_empty() && (source != ScreenSource::Game || c.capture));
        if !can {
            return Err(Refusal::NoEncoder);
        }
        self.hosting = Some(Hosting {
            title,
            source,
            game,
            worker: None,
        });
        self.sync_dialers();
        Ok(())
    }

    fn stop_sharing(&mut self, reason: &'static str) {
        let Some(hosting) = self.hosting.take() else {
            return;
        };
        if let Some(worker) = hosting.worker {
            let _ = worker.to.send(ToWorker::Stop);
        }
        self.dialers.clear();
        let keys: Vec<PeerKey> = self
            .peers
            .keys()
            .filter(|(_, side)| *side == Side::Out)
            .copied()
            .collect();
        for key in keys {
            self.end_peer(key, code::STOPPED, Some(reason));
        }
    }

    fn watch(&mut self, account: u64) -> Result<(), Refusal> {
        if self.watching.is_some() {
            return Err(Refusal::Busy);
        }
        let key = self
            .peers
            .iter()
            .find(|((_, side), p)| *side == Side::In && p.account == account && p.hello.is_some())
            .map(|(key, _)| *key)
            .ok_or(Refusal::NotFound)?;
        if self
            .workers
            .capabilities()
            .is_none_or(|c| c.decode.is_empty())
        {
            return Err(Refusal::NoDecoder);
        }
        let title = self
            .peers
            .get(&key)
            .and_then(|p| p.hello.as_ref())
            .map(|h| h.title.clone())
            .unwrap_or_default();
        let port = self
            .workers
            .launch(
                Role::Guest,
                ScreenSource::Game,
                &title,
                &GameWindow::default(),
            )
            .map_err(|error| {
                tracing::warn!("screen: viewer worker: {error}");
                Refusal::NoDecoder
            })?;
        self.generation += 1;
        let generation = self.generation;
        let forward = self.forward(WorkerRole::Viewer, generation, port.from);
        let now = Instant::now();
        self.watching = Some(Watching {
            key,
            generation,
            to: port.to,
            stats: WorkerStats::default(),
            live: false,
            last_progress: now,
            since: now,
            _forward: forward,
        });
        if let Some(peer) = self.peers.get_mut(&key) {
            peer.watching = true;
            let _ = peer.writer.send(Message::Watch {});
        }
        Ok(())
    }

    fn leave(&mut self) {
        let Some(watching) = self.watching.take() else {
            return;
        };
        let _ = watching.to.send(ToWorker::Stop);
        if let Some(peer) = self.peers.get_mut(&watching.key) {
            peer.watching = false;
            let _ = peer.writer.send(Message::Unwatch {});
        }
    }

    // Connections

    /// Dials every member that is not dialed, while sharing; drops dialers otherwise.
    fn sync_dialers(&mut self) {
        if self.hosting.is_none() {
            self.dialers.clear();
            return;
        }
        let members = &self.members;
        self.dialers
            .retain(|id, dialer| members.get(id).is_some_and(|m| m.relay == dialer.relay));
        for (id, member) in &self.members {
            if self.dialers.contains_key(id) {
                continue;
            }
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

    fn set_members(&mut self, members: Members) {
        self.members = members;
        self.members.remove(&self.me);
        let members = &self.members;
        let gone: Vec<PeerKey> = self
            .peers
            .iter()
            .filter(|((id, _), peer)| members.get(id).is_none_or(|m| m.account != peer.account))
            .map(|(key, _)| *key)
            .collect();
        for key in gone {
            self.end_peer(key, code::LEFT_PARTY, None);
        }
        self.sync_dialers();
    }

    /// A connection arrived: it stays only when it is a current member's.
    fn accept(&mut self, connection: Connection) {
        let id = connection.remote_id();
        if !self.members.contains_key(&id) {
            tracing::debug!("screen: refused {}", id.fmt_short());
            connection.close(code::NOT_IN_PARTY.into(), b"not in the party");
            return;
        }
        self.attach((id, Side::In), connection);
    }

    fn attach(&mut self, key: PeerKey, connection: Connection) {
        let Some(account) = self.members.get(&key.0).map(|m| m.account) else {
            connection.close(code::NOT_IN_PARTY.into(), b"not in the party");
            return;
        };
        if key.1 == Side::Out && self.hosting.is_none() {
            connection.close(code::STOPPED.into(), b"not sharing");
            return;
        }
        if let Some(old) = self.peers.remove(&key) {
            old.connection.close(code::REPLACED.into(), b"replaced");
            if self.watching.as_ref().is_some_and(|w| w.key == key) {
                self.leave();
            }
        }
        let stable = connection.stable_id();
        let (writer, queue) = PeerTx::new(MAX_QUEUED_MESSAGES, {
            let connection = connection.clone();
            move || connection.close(code::STALLED.into(), b"control stream stalled")
        });
        // Our hello goes first on the stream.
        let caps = self.workers.capabilities().unwrap_or_default();
        let hello = match key.1 {
            Side::Out => wire::Hello {
                version: wire::VERSION,
                role: Role::Host,
                encode: caps.encode,
                decode: Vec::new(),
                title: self
                    .hosting
                    .as_ref()
                    .map(|h| h.title.clone())
                    .unwrap_or_default(),
            },
            Side::In => wire::Hello {
                version: wire::VERSION,
                role: Role::Guest,
                encode: Vec::new(),
                decode: caps.decode,
                title: String::new(),
            },
        };
        let _ = writer.send(Message::Hello(hello));
        let control = Guard(tokio::spawn(control_stream(
            key,
            stable,
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
                    let _ = net.try_send(Net::Datagram(key, stable, datagram));
                }
                let _ = net.send(Net::Closed(key, stable)).await;
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
                    if net.send(Net::Path(key, stable, link)).await.is_err() {
                        return;
                    }
                }
            }))
        };
        self.peers.insert(
            key,
            Peer {
                connection,
                stable,
                account,
                link: Link::Connecting,
                rtt_ms: None,
                writer,
                hello: None,
                watching: false,
                rate: None,
                loss_percent: 0,
                last_ping: Instant::now()
                    .checked_sub(PING_EVERY)
                    .unwrap_or_else(Instant::now),
                messages: (Instant::now(), 0),
                _tasks: [control, datagrams, paths],
            },
        );
    }

    /// Closes a connection after telling the other side why, when there is a reason.
    fn end_peer(&mut self, key: PeerKey, close_code: u32, reason: Option<&str>) {
        let Some(peer) = self.peers.remove(&key) else {
            return;
        };
        if let Some(reason) = reason {
            let _ = peer.writer.send(Message::Stop(wire::Stop {
                reason: reason.to_owned(),
            }));
        }
        let grace = if reason.is_some() {
            CLOSE_GRACE
        } else {
            Duration::ZERO
        };
        // The peer's tasks live until the connection is closed, so that the last control
        // message is written first.
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            peer.connection.close(close_code.into(), b"screen");
        });
        self.peer_gone(key);
    }

    /// Bookkeeping after a peer is gone, however it went.
    fn peer_gone(&mut self, key: PeerKey) {
        if let Some(watching) = self.watching.take_if(|w| w.key == key) {
            let _ = watching.to.send(ToWorker::Stop);
        }
        if key.1 == Side::Out {
            self.watchers_changed();
        }
    }

    // Messages from the network and the workers

    fn network(&mut self, message: Net) {
        match message {
            Net::Dialed(id, connection) => self.attach((id, Side::Out), connection),
            Net::Datagram(key, stable, bytes) => {
                if !self.current(key, stable) {
                    return;
                }
                // Only a viewer takes datagrams, from the stream it watches.
                if key.1 == Side::In
                    && let Some(watching) = self.watching.as_mut().filter(|w| w.key == key)
                {
                    let _ = watching.to.send(ToWorker::Datagram(bytes.to_vec()));
                }
            }
            Net::Control(key, stable, message) => {
                if self.current(key, stable) {
                    self.on_message(key, message);
                }
            }
            Net::ControlBroken(key, stable) => {
                if self.current(key, stable) {
                    self.end_peer(key, code::PROTOCOL, None);
                }
            }
            Net::Path(key, stable, link) => {
                if let Some(peer) = self.peers.get_mut(&key).filter(|p| p.stable == stable) {
                    peer.link = link;
                    if let Some(rate) = &mut peer.rate {
                        rate.set_path(path_kind(link));
                    }
                }
            }
            Net::Closed(key, stable) => {
                if self.current(key, stable) {
                    self.peers.remove(&key);
                    self.peer_gone(key);
                }
            }
            Net::Worker(role, generation, message) => self.on_worker(role, generation, message),
        }
        self.report(false);
    }

    fn current(&self, key: PeerKey, stable: usize) -> bool {
        self.peers.get(&key).is_some_and(|p| p.stable == stable)
    }

    fn on_message(&mut self, key: PeerKey, message: Message) {
        let now = Instant::now();
        let now_us = self.now_us();
        let flooded = {
            let Some(peer) = self.peers.get_mut(&key) else {
                return;
            };
            if now.duration_since(peer.messages.0) >= Duration::from_secs(1) {
                peer.messages = (now, 0);
            }
            peer.messages.1 += 1;
            peer.messages.1 > MESSAGES_PER_SECOND
        };
        if flooded {
            self.end_peer(key, code::PROTOCOL, None);
            return;
        }
        match (key.1, message) {
            (_, Message::Hello(hello)) => {
                // The streamer must be a host and the viewer a guest.
                let expected = if key.1 == Side::In {
                    Role::Host
                } else {
                    Role::Guest
                };
                if hello.version != wire::VERSION || hello.role != expected {
                    self.end_peer(key, code::PROTOCOL, None);
                    return;
                }
                if let Some(peer) = self.peers.get_mut(&key) {
                    peer.hello = Some(hello);
                }
            }
            (_, Message::Ping(ping)) => {
                if let Some(peer) = self.peers.get(&key) {
                    let _ = peer.writer.send(Message::Pong(wire::Pong {
                        t0: ping.t0,
                        t1: now_us,
                        t2: now_us,
                    }));
                }
            }
            (_, Message::Pong(pong)) => {
                let (rtt, _) = wire::clock_estimate(pong.t0, pong.t1, pong.t2, now_us);
                if let Some(peer) = self.peers.get_mut(&key) {
                    peer.rtt_ms = Some((rtt.max(0) / 1000) as u32);
                }
            }
            // The streamer's side
            (Side::Out, Message::Watch {}) => self.viewer_watches(key),
            (Side::Out, Message::Unwatch {}) => {
                if let Some(peer) = self.peers.get_mut(&key) {
                    peer.watching = false;
                    peer.rate = None;
                }
                self.watchers_changed();
            }
            (Side::Out, Message::Report(report)) => self.viewer_reports(key, report),
            (Side::Out, Message::Key(_)) => {
                if let Some(worker) = self.hosting.as_ref().and_then(|h| h.worker.as_ref()) {
                    let _ = worker.to.send(ToWorker::KeyFrame);
                }
            }
            // The viewer's side
            (Side::In, Message::Config(config)) => {
                // Only a codec this PC offered to decode may reach the worker; the streamer
                // chose it from that list, so anything else is not what was agreed.
                let decodes = self.workers.capabilities().unwrap_or_default().decode;
                if !decodes_codec(&decodes, &config.codec) {
                    self.end_peer(key, code::PROTOCOL, None);
                    return;
                }
                let epoch = config.epoch;
                if let Some(watching) = self.watching.as_ref().filter(|w| w.key == key) {
                    let _ = watching.to.send(ToWorker::Start(Start {
                        epoch: config.epoch,
                        codec: config.codec,
                        width: config.width,
                        height: config.height,
                        fps: config.fps,
                        kbps: config.kbps,
                        parity_percent: 0,
                        max_payload: 0,
                    }));
                    // Whatever came before the setup was dropped: a key frame starts the picture.
                    if let Some(peer) = self.peers.get(&key) {
                        let _ = peer
                            .writer
                            .send(Message::Key(wire::Key { epoch, frame: 0 }));
                    }
                }
            }
            (Side::In, Message::Stop(_)) => {
                // The stream ended or refused this viewer. The connection closes by itself.
                if self.watching.as_ref().is_some_and(|w| w.key == key) {
                    self.leave_quietly();
                }
                if let Some(peer) = self.peers.get_mut(&key) {
                    peer.watching = false;
                }
            }
            _ => {}
        }
    }

    fn leave_quietly(&mut self) {
        if let Some(watching) = self.watching.take() {
            let _ = watching.to.send(ToWorker::Stop);
        }
    }

    fn on_worker(&mut self, role: WorkerRole, generation: u64, message: FromWorker) {
        match role {
            WorkerRole::Streamer => {
                let Some(worker) = self
                    .hosting
                    .as_mut()
                    .and_then(|h| h.worker.as_mut())
                    .filter(|w| w.generation == generation)
                else {
                    return;
                };
                match message {
                    FromWorker::Datagram(bytes) => {
                        for ((_, side), peer) in &self.peers {
                            if *side == Side::Out && peer.watching {
                                // A datagram that does not fit or a connection that just
                                // closed is a lost packet.
                                let _ = peer.connection.send_datagram(Bytes::from(bytes.clone()));
                            }
                        }
                    }
                    FromWorker::Stats(stats) => worker.stats = stats,
                    FromWorker::Ended(reason) => {
                        tracing::info!("screen: streamer worker ended: {reason}");
                        let _ = self.out.send(Output::Notice {
                            code: "screen_worker",
                            message: format!("Sharing your screen stopped: {reason}"),
                        });
                        self.stop_sharing("ended");
                    }
                    // Share screen has no pad and no ring: nothing of the kind is acted on.
                    FromWorker::Report(_)
                    | FromWorker::Key(_)
                    | FromWorker::Pad(_)
                    | FromWorker::RingReady(_) => {}
                }
            }
            WorkerRole::Viewer => {
                let Some(watching) = self
                    .watching
                    .as_mut()
                    .filter(|w| w.generation == generation)
                else {
                    return;
                };
                let key = watching.key;
                match message {
                    FromWorker::Report(report) => {
                        if let Some(peer) = self.peers.get(&key) {
                            let _ = peer.writer.send(Message::Report(report));
                        }
                    }
                    FromWorker::Key(request) => {
                        if let Some(peer) = self.peers.get(&key) {
                            let _ = peer.writer.send(Message::Key(request));
                        }
                    }
                    FromWorker::Stats(stats) => {
                        if stats.frames > watching.stats.frames {
                            watching.last_progress = Instant::now();
                            watching.live = true;
                        }
                        watching.stats = stats;
                    }
                    FromWorker::Ended(reason) => {
                        tracing::info!("screen: viewer worker ended: {reason}");
                        self.leave();
                    }
                    FromWorker::Datagram(_) | FromWorker::RingReady(_) => {}
                    // Share screen is watch only (ADR 0023): a pad state is dropped here.
                    FromWorker::Pad(_) => {}
                }
            }
        }
    }

    // The streamer's worker

    /// A viewer said `watch`.
    fn viewer_watches(&mut self, key: PeerKey) {
        let Some(hosting) = &self.hosting else {
            return;
        };
        let watchers = self
            .peers
            .iter()
            .filter(|((_, side), p)| *side == Side::Out && p.watching)
            .count();
        if watchers >= MAX_VIEWERS {
            if let Some(peer) = self.peers.get(&key) {
                let _ = peer.writer.send(Message::Stop(wire::Stop {
                    reason: "full".into(),
                }));
            }
            return;
        }
        let caps = self.workers.capabilities().unwrap_or_default();
        let viewer_decodes = self
            .peers
            .get(&key)
            .and_then(|p| p.hello.as_ref())
            .map(|h| h.decode.clone())
            .unwrap_or_default();
        let codec = pick_codec(&caps.encode, &viewer_decodes, hosting.source);
        let running = hosting.worker.as_ref().map(|w| w.start.codec.clone());
        let usable = match (&codec, &running) {
            (Some(_), None) => true,
            (_, Some(running)) => viewer_decodes.iter().any(|c| &c.codec == running),
            _ => false,
        };
        let was_running = running.is_some();
        let Some(peer) = self.peers.get_mut(&key) else {
            return;
        };
        if !usable {
            let _ = peer.writer.send(Message::Stop(wire::Stop {
                reason: "no_encoder".into(),
            }));
            return;
        }
        let now_us = self.clock.elapsed().as_micros() as u64;
        let controller = RateController::new(RateConfig::default(), path_kind(peer.link), now_us);
        peer.rate = Some(controller);
        peer.watching = true;
        self.watchers_changed();
        // A viewer that joins a running stream needs a key frame and the config.
        let late = self
            .hosting
            .as_ref()
            .and_then(|h| h.worker.as_ref())
            .filter(|_| was_running);
        if let Some(worker) = late {
            let start = worker.start.clone();
            let _ = worker.to.send(ToWorker::KeyFrame);
            if let Some(peer) = self.peers.get(&key) {
                let _ = peer.writer.send(config_message(&start));
            }
        }
    }

    /// The set of watchers changed: starts the worker for the first, ends it after the last.
    fn watchers_changed(&mut self) {
        let Some(hosting) = &mut self.hosting else {
            return;
        };
        let watchers: Vec<PeerKey> = self
            .peers
            .iter()
            .filter(|((_, side), p)| *side == Side::Out && p.watching)
            .map(|(key, _)| *key)
            .collect();
        if watchers.is_empty() {
            if let Some(worker) = hosting.worker.take() {
                let _ = worker.to.send(ToWorker::Stop);
            }
            return;
        }
        if hosting.worker.is_some() {
            return;
        }
        let caps = self.workers.capabilities().unwrap_or_default();
        let decodes = watchers
            .iter()
            .filter_map(|k| self.peers.get(k))
            .filter_map(|p| p.hello.as_ref())
            .map(|h| h.decode.clone())
            .next()
            .unwrap_or_default();
        let Some(codec) = pick_codec(&caps.encode, &decodes, hosting.source) else {
            return;
        };
        let port =
            match self
                .workers
                .launch(Role::Host, hosting.source, &hosting.title, &hosting.game)
            {
                Ok(port) => port,
                Err(error) => {
                    tracing::warn!("screen: streamer worker: {error}");
                    let _ = self.out.send(Output::Notice {
                        code: "screen_worker",
                        message: format!("Sharing your screen could not start: {error}"),
                    });
                    self.stop_sharing("no_encoder");
                    return;
                }
            };
        self.generation += 1;
        let generation = self.generation;
        let target = self.best_target(&watchers);
        let start = Start {
            epoch: 0,
            codec,
            width: target.width,
            height: target.height,
            fps: target.fps,
            kbps: target.kbps,
            parity_percent: target.parity_percent,
            max_payload: self.max_payload(&watchers),
        };
        let _ = port.to.send(ToWorker::Start(start.clone()));
        let forward = self.forward(WorkerRole::Streamer, generation, port.from);
        if let Some(hosting) = &mut self.hosting {
            hosting.worker = Some(HostWorker {
                generation,
                to: port.to,
                start: start.clone(),
                stats: WorkerStats::default(),
                _forward: forward,
            });
        }
        for key in &watchers {
            if let Some(peer) = self.peers.get(key) {
                let _ = peer.writer.send(config_message(&start));
            }
        }
    }

    /// The datagram payload that fits every watcher's connection.
    fn max_payload(&self, watchers: &[PeerKey]) -> usize {
        watchers
            .iter()
            .filter_map(|k| self.peers.get(k))
            .filter_map(|p| p.connection.max_datagram_size())
            .map(|size| size.saturating_sub(20))
            .min()
            .unwrap_or(DEFAULT_PAYLOAD)
            .max(anyplay_share::packetiser::MIN_PAYLOAD)
    }

    /// The lowest target among the watchers: the stream follows the weakest.
    fn best_target(&self, watchers: &[PeerKey]) -> Target {
        watchers
            .iter()
            .filter_map(|k| self.peers.get(k))
            .filter_map(|p| p.rate.as_ref())
            .map(|r| (r.budget_kbps(), r.target()))
            .min_by_key(|(budget, _)| *budget)
            .map(|(_, target)| target)
            .unwrap_or_else(|| {
                RateController::new(RateConfig::default(), PathKind::Relay, 0).target()
            })
    }

    /// A viewer's report: feeds its controller and passes the weakest target to the worker.
    fn viewer_reports(&mut self, key: PeerKey, report: wire::Report) {
        let now_us = self.now_us();
        let Some(peer) = self.peers.get_mut(&key) else {
            return;
        };
        let total = u64::from(report.received) + u64::from(report.lost);
        peer.loss_percent = (u64::from(report.lost) * 100)
            .checked_div(total)
            .map_or(0, |percent| percent.min(100)) as u8;
        let Some(rate) = &mut peer.rate else {
            return;
        };
        rate.on_input(&RateInput {
            now_us,
            received: report.received,
            lost: report.lost,
            delay_trend_us: report.delay_trend_us,
            send_queue_frames: 0,
            delivery_kbps: None,
        });
        let watchers: Vec<PeerKey> = self
            .peers
            .iter()
            .filter(|((_, side), p)| *side == Side::Out && p.watching)
            .map(|(key, _)| *key)
            .collect();
        let target = self.best_target(&watchers);
        let Some(worker) = self.hosting.as_mut().and_then(|h| h.worker.as_mut()) else {
            return;
        };
        let shape_changed = (target.width, target.height, target.fps)
            != (worker.start.width, worker.start.height, worker.start.fps);
        if shape_changed {
            let mut start = worker.start.clone();
            start.epoch = start.epoch.wrapping_add(1);
            start.width = target.width;
            start.height = target.height;
            start.fps = target.fps;
            start.kbps = target.kbps;
            start.parity_percent = target.parity_percent;
            worker.start = start.clone();
            let _ = worker.to.send(ToWorker::Start(start.clone()));
            for key in &watchers {
                if let Some(peer) = self.peers.get(key) {
                    let _ = peer.writer.send(config_message(&start));
                }
            }
        } else if (target.kbps, target.parity_percent)
            != (worker.start.kbps, worker.start.parity_percent)
        {
            worker.start.kbps = target.kbps;
            worker.start.parity_percent = target.parity_percent;
            let _ = worker.to.send(ToWorker::Rate {
                kbps: target.kbps,
                parity_percent: target.parity_percent,
            });
        }
    }

    /// Moves a worker's messages into the hub's inbox, tagged with who sent them.
    fn forward(
        &self,
        role: WorkerRole,
        generation: u64,
        mut from: mpsc::Receiver<FromWorker>,
    ) -> Guard {
        let net = self.net.clone();
        Guard(tokio::spawn(async move {
            while let Some(message) = from.recv().await {
                // A worker that is ahead of the hub waits; video is never queued past the
                // worker's own limits.
                if net
                    .send(Net::Worker(role, generation, message))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            let _ = net
                .send(Net::Worker(
                    role,
                    generation,
                    FromWorker::Ended("the worker closed".into()),
                ))
                .await;
        }))
    }

    // Time

    fn tick(&mut self) {
        let now = Instant::now();
        for peer in self.peers.values_mut() {
            if now.duration_since(peer.last_ping) >= PING_EVERY {
                peer.last_ping = now;
                let t0 = self.clock.elapsed().as_micros() as u64;
                let _ = peer.writer.send(Message::Ping(wire::Ping { t0 }));
            }
        }
        if let Some(watching) = &self.watching {
            let quiet_since = if watching.live {
                watching.last_progress
            } else {
                watching.since
            };
            if now.duration_since(quiet_since) >= GIVE_UP_AFTER {
                self.leave();
            }
        }
        self.report(false);
    }

    // The view

    fn view(&self) -> View {
        let link_of = |key: PeerKey| self.peers.get(&key).map(|p| p.link);
        let hosting = self.hosting.as_ref().map(|hosting| {
            let worker = hosting.worker.as_ref();
            let mut viewers: Vec<ScreenViewer> = self
                .peers
                .iter()
                .filter(|((_, side), _)| *side == Side::Out)
                .map(|(_, p)| ScreenViewer {
                    account: AccountId(p.account),
                    watching: p.watching,
                    link: Some(p.link),
                    rtt_ms: p.rtt_ms,
                    loss_percent: p.loss_percent,
                })
                .collect();
            viewers.sort_by_key(|v| v.account.0);
            let stats = worker.map(|w| w.stats).unwrap_or_default();
            ScreenHosting {
                phase: match worker {
                    Some(w) if w.stats.frames > 0 => ScreenPhase::Live,
                    _ => ScreenPhase::Connecting,
                },
                title: hosting.title.clone(),
                codec: worker.map(|w| w.start.codec.clone()),
                stats: ScreenStats {
                    width: stats.width,
                    height: stats.height,
                    fps: stats.fps,
                    kbps: stats.kbps,
                },
                viewers,
            }
        });
        let watched = self.watching.as_ref().map(|w| w.key);
        let mut offers: Vec<ScreenOffer> = self
            .peers
            .iter()
            .filter(|(key, p)| key.1 == Side::In && Some(**key) != watched && p.hello.is_some())
            .map(|(key, p)| ScreenOffer {
                account: AccountId(p.account),
                title: p
                    .hello
                    .as_ref()
                    .map(|h| h.title.clone())
                    .unwrap_or_default(),
                link: link_of(*key),
            })
            .collect();
        offers.sort_by_key(|o| o.account.0);
        let watching = self.watching.as_ref().and_then(|w| {
            let peer = self.peers.get(&w.key)?;
            let now = Instant::now();
            Some(ScreenWatching {
                account: AccountId(peer.account),
                title: peer
                    .hello
                    .as_ref()
                    .map(|h| h.title.clone())
                    .unwrap_or_default(),
                phase: if !w.live {
                    ScreenPhase::Connecting
                } else if now.duration_since(w.last_progress) >= STALL_AFTER {
                    ScreenPhase::Stalled
                } else {
                    ScreenPhase::Live
                },
                link: Some(peer.link),
                rtt_ms: peer.rtt_ms,
                stats: ScreenStats {
                    width: w.stats.width,
                    height: w.stats.height,
                    fps: w.stats.fps,
                    kbps: w.stats.kbps,
                },
                shown_fps: w.stats.fps,
            })
        });
        View {
            hosting,
            offers,
            watching,
        }
    }

    /// Tells the service when the view changed: at the next opportunity for its shape, and
    /// twice a second for its numbers.
    fn report(&mut self, force: bool) {
        let view = self.view();
        if self.last_view.as_ref() == Some(&view) {
            return;
        }
        let shape_changed = self.last_view.as_ref().map(View::shape) != Some(view.shape());
        let due = self.last_numbers.elapsed() >= NUMBERS_PACE;
        if shape_changed || due || force {
            self.last_numbers = Instant::now();
            self.last_view = Some(view.clone());
            let _ = self.out.send(Output::View(view));
        }
    }
}

fn path_kind(link: Link) -> PathKind {
    match link {
        Link::Direct => PathKind::Direct,
        // Until the path is known, assume the narrower one.
        Link::Relay | Link::Connecting => PathKind::Relay,
    }
}

fn config_message(start: &Start) -> Message {
    Message::Config(wire::Config {
        epoch: start.epoch,
        codec: start.codec.clone(),
        width: start.width,
        height: start.height,
        fps: start.fps,
        kbps: start.kbps,
        mode: wire::Mode::Watch,
    })
}

/// Whether `codec` is one of the decoders this PC can run.
fn decodes_codec(decode: &[CodecCaps], codec: &str) -> bool {
    decode.iter().any(|c| c.codec == codec)
}

/// AV1, then H.264, then whatever else both have, in the streamer's order.
/// The codec a stream uses: AV1, else H.264, else (only for the test pattern) the test codec.
/// The test codec is lossless and huge; it is never what a player's game is streamed with.
fn pick_codec(encode: &[CodecCaps], decode: &[CodecCaps], source: ScreenSource) -> Option<String> {
    let both = |name: &str| {
        encode.iter().any(|c| c.codec == name) && decode.iter().any(|c| c.codec == name)
    };
    for preferred in ["av1", "h264"] {
        if both(preferred) {
            return Some(preferred.to_owned());
        }
    }
    (source == ScreenSource::TestPattern && both("test")).then(|| "test".to_owned())
}

/// The control stream of one connection: the dialing side opens it, the other accepts it. Lines
/// from `queue` are written, lines read are parsed and handed to the hub.
async fn control_stream(
    key: PeerKey,
    stable: usize,
    connection: Connection,
    mut queue: mpsc::Receiver<Message>,
    net: mpsc::Sender<Net>,
) {
    let opened = match key.1 {
        // We dialed, so we open the stream; the first line makes the other side see it.
        Side::Out => connection.open_bi().await,
        Side::In => connection.accept_bi().await,
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
                            .send(Net::Control(key, stable, message))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Ok(None) => {}
                    Err(_) => {
                        let _ = reader_net.send(Net::ControlBroken(key, stable)).await;
                        return;
                    }
                }
            }
            if buffer.len() > wire::MAX_LINE {
                let _ = reader_net.send(Net::ControlBroken(key, stable)).await;
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
            Ok(Err(error)) => tracing::debug!("screen dial {}: {error}", id.fmt_short()),
            Err(_) => tracing::debug!("screen dial {}: no answer", id.fmt_short()),
        }
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(LONGEST_PAUSE);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn a_viewer_takes_only_a_codec_it_can_decode() {
        let caps = |codec: &str| CodecCaps {
            codec: codec.to_owned(),
            hardware: false,
            max_width: 1920,
            max_height: 1080,
            max_fps: 60,
        };
        let decode = vec![caps("h264")];
        assert!(decodes_codec(&decode, "h264"));
        assert!(!decodes_codec(&decode, "av1"));
        assert!(!decodes_codec(&decode, "../../evil"));
        assert!(!decodes_codec(&[], "h264"));
    }

    #[test]
    fn a_peer_that_stops_reading_ends_its_connection_instead_of_growing_the_queue() {
        let ended = Arc::new(AtomicUsize::new(0));
        let (tx, mut rx) = PeerTx::new(MAX_QUEUED_MESSAGES, {
            let ended = ended.clone();
            move || {
                ended.fetch_add(1, Ordering::SeqCst);
            }
        });
        for _ in 0..MAX_QUEUED_MESSAGES {
            assert!(tx.send(Message::Watch {}));
        }
        assert_eq!(ended.load(Ordering::SeqCst), 0);
        // A hostile peer keeps provoking answers while it reads nothing.
        for _ in 0..100_000 {
            assert!(!tx.send(Message::Watch {}));
        }
        assert!(ended.load(Ordering::SeqCst) >= 1);
        let mut queued = 0;
        while rx.try_recv().is_ok() {
            queued += 1;
        }
        assert_eq!(
            queued, MAX_QUEUED_MESSAGES,
            "the queue never passed its cap"
        );
    }

    #[test]
    fn a_connection_that_is_already_gone_is_not_an_overflow() {
        let ended = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = PeerTx::new(4, {
            let ended = ended.clone();
            move || {
                ended.fetch_add(1, Ordering::SeqCst);
            }
        });
        drop(rx);
        assert!(!tx.send(Message::Watch {}));
        assert_eq!(ended.load(Ordering::SeqCst), 0);
    }
}
