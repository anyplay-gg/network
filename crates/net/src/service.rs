//! The net service's single owner of state.
//!
//! Everything that changes what the player sees runs here, one input at a time: commands from
//! the launcher, events from the cloud, frames from the game's bridge and changes of the
//! tunnels. Requests to the cloud run in their own tasks and come back as continuations, so the
//! loop never waits on the network. Game packets never pass through here (see `traffic`).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyplay_api::{
    AccountId, Activity, CodeAnswer, CreateSession, Device, ErrorCode, Event, FriendLists,
    Invitation, JoinSession, MAX_DAY_MINUTES, MAX_PLAYTIME_UPLOAD, Me, Platform, PlaytimeEntry,
    PresenceRequest, Report, Session, SessionKind, VersionRule, Visibility, check_handle,
    day_number, is_title_id,
};
use anyplay_bridge_protocol::{
    self as bridge, ActivateResult, DialogResult, Frame, Message, PeerInfoReply, PeerState,
    Transport, WebReply, Welcome,
};
use anyplay_control::{
    self as control, Accepted, Answer, CodeAccepted, CodeResult, Command, Done, EmailSent, Failure,
    FriendAdded, GameStarted, Hello, Invited, Opened, Output, Pending, SignedIn,
};
use anyplay_voice::Engine;
use iroh::{EndpointId, RelayUrl, SecretKey};
use tokio::io::AsyncReadExt as _;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use crate::addresses::PeerAddresses;
use crate::answers::{Answers, is_older};
use crate::bridge::BridgeInput;
use crate::browser::{self, Callback};
use crate::cloud::{Cloud, CloudError, now};
use crate::credentials::{Saved, Store};
use crate::events::{self, EventInput};
use crate::observe::{self, Observer};
use crate::screen::{self, WorkerSet};
use crate::traffic::Traffic;
use crate::tunnel::{self, Link, LinkChange, LinkTotals, RelayAccess, Tunnels};
use crate::voice::{self, Audio, Hub};
use crate::webapi;

mod discord;
mod share;

/// How the service is set up; the cloud comes later, with the launcher's hello.
#[derive(Clone, Debug)]
pub struct Config {
    /// Where the endpoint key, a file-kept sign-in and observation logs live.
    pub data: PathBuf,
    /// Keep the sign-in in a file even when the system has a credential store.
    pub file_credentials: bool,
    /// Root certificates to trust for relays besides the usual ones.
    pub relay_roots: Vec<rustls::pki_types::CertificateDer<'static>>,
    /// Bind tunnels only to this address, as tests do on loopback.
    pub bind: Option<std::net::SocketAddr>,
    /// Every tunnel goes through a relay, so other players never learn this PC's addresses.
    pub relay_only: bool,
    /// The name of this device in the account's device list.
    pub device_name: String,
    /// The microphones and speakers party voice uses.
    pub audio: Audio,
    /// The share worker that captures, encodes, decodes and shows streams.
    pub workers: WorkerSet,
    /// Discord's Social SDK: off unless the library is there (ADR 0022).
    pub discord: anyplay_discord::Backend,
}

type Continuation = Box<dyn FnOnce(&mut Service) + Send>;

enum Input {
    Command(u64, Command),
    BadCommand(Option<u64>),
    ControlClosed,
    Events(u64, EventInput),
    Bridge(BridgeInput),
    Link(LinkChange),
    /// What a net report of the tunnels of that generation found.
    Reach(u64, tunnel::Reach),
    /// A report of the voice hub of that generation.
    Voice(u64, voice::Report),
    /// A report of the screen hub of that generation.
    Screen(u64, screen::Output),
    /// A report of the share play hub of that generation.
    Play(u64, crate::play::Output),
    Done(Continuation),
    Discord(anyplay_discord::Event),
    Tick,
    Stop,
}

/// How long the player stays signed in for the game while the cloud cannot be reached.
const OFFLINE_GRACE: Duration = Duration::from_secs(30);
/// How often the launcher hears about friends at most.
const FRIENDS_PACE: Duration = Duration::from_millis(250);
/// The least time between two `feed` events.
const FEED_PACE: Duration = Duration::from_secs(2);
/// How long a quitting service waits for its last requests to the cloud.
const GOODBYE: Duration = Duration::from_secs(3);
/// The most a member can be turned up to, in percent.
const MAX_PEER_VOLUME_PERCENT: u16 = 200;
/// The API group the network answers in its own format, for the fake game and for homebrew.
pub const OWN_API_GROUP: &str = "anyplay";

struct GameRun {
    title_id: String,
    title: String,
    version: String,
    build: String,
    token: Vec<u8>,
    join: Option<String>,
    intent_sent: bool,
    /// Sessions this game run created or joined; it leaves them when the game ends.
    sessions: HashSet<String>,
}

/// Share screen as it runs: the hub exists while the player is in a party.
#[derive(Default)]
struct ScreenSide {
    hub: Option<screen::Hub>,
    generation: u64,
    view: screen::View,
}

/// Party voice as the player set it and as it runs.
#[derive(Default)]
struct VoiceSide {
    /// The player switched voice on. The microphone opens only while this is true and the
    /// player is in a party.
    enabled: bool,
    settings: voice::Settings,
    /// The chosen devices by name; none is the system's default.
    input: Option<String>,
    output: Option<String>,
    hub: Option<Hub>,
    /// The devices are being opened.
    opening: bool,
    /// Counts openings and restarts, so that a report or an opening of an older one is dropped.
    generation: u64,
    report: Option<voice::Report>,
    /// What this device last told the party about itself, with the party's ID.
    announced: Option<(String, anyplay_api::PartyEndpoint)>,
}

struct BridgeLink {
    connection: u64,
    frames: UnboundedSender<Frame>,
    activated: HashSet<String>,
}

struct Dialog {
    connection: u64,
    request: u32,
    session: String,
}

/// The peers by endpoint, leaving out an endpoint that is this device's own or that two
/// different accounts name: whose it is cannot be told, so neither gets it.
fn unambiguous_endpoints<T>(
    named: Vec<(EndpointId, u64, T)>,
    own: EndpointId,
) -> HashMap<EndpointId, T> {
    let mut seen: HashMap<EndpointId, (u64, Option<T>)> = HashMap::new();
    for (endpoint, account, peer) in named {
        if endpoint == own {
            continue;
        }
        match seen.get_mut(&endpoint) {
            None => {
                seen.insert(endpoint, (account, Some(peer)));
            }
            Some((first, kept)) if *first != account => *kept = None,
            Some(_) => {}
        }
    }
    seen.into_iter()
        .filter_map(|(endpoint, (_, peer))| peer.map(|p| (endpoint, p)))
        .collect()
}

/// Whether the session's record of this endpoint should be pushed now. Pushing the same
/// endpoint and relay twice never helps: if the record is still stale after the first push, the
/// cloud answers differently than asked, and asking again would loop.
fn endpoint_push_due(
    pushed: &mut HashMap<String, (String, Option<String>)>,
    session: &str,
    stale: bool,
    endpoint: &str,
    relay: &Option<String>,
) -> bool {
    if !stale {
        pushed.remove(session);
        return false;
    }
    if pushed
        .get(session)
        .is_some_and(|(e, r)| e == endpoint && r == relay)
    {
        return false;
    }
    pushed.insert(session.to_owned(), (endpoint.to_owned(), relay.clone()));
    true
}

/// Whether `game` is the run that began with `token`.
fn game_is(game: Option<&GameRun>, token: &[u8]) -> bool {
    game.is_some_and(|g| g.token == token)
}

/// How many invitation dialogs may wait for the player at once.
const MAX_DIALOGS: usize = 8;

/// Keeps a new dialog and returns its id, or `None` when [`MAX_DIALOGS`] already wait.
fn admit_dialog(dialogs: &mut HashMap<u64, Dialog>, next: &mut u64, dialog: Dialog) -> Option<u64> {
    if dialogs.len() >= MAX_DIALOGS {
        return None;
    }
    let id = *next;
    *next += 1;
    dialogs.insert(id, dialog);
    Some(id)
}

enum PendingSignIn {
    Code {
        challenge: String,
    },
    Handle {
        signup: String,
    },
    /// The system browser has the sign-in; the loopback listener waits for it.
    Browser,
}

pub struct Service {
    config: Config,
    inbox: UnboundedSender<Input>,
    out: UnboundedSender<String>,
    hello: bool,
    cloud: Option<Cloud>,
    store: Option<Store>,
    me: Option<Me>,
    pending: Option<PendingSignIn>,
    /// The loopback listener and the task that finishes the browser sign-in.
    browser: Option<(JoinHandle<()>, JoinHandle<()>)>,
    link: control::Cloud,
    offline_since: Option<Instant>,
    /// The cloud refused this client as too old: told to the launcher once per run.
    client_too_old: Once,
    events: Option<(u64, JoinHandle<()>)>,
    next_events: u64,
    /// The address the cloud named in `welcome`: the player's own before it is in a session.
    address: Option<Ipv4Addr>,
    /// What the game calls each peer, over all sessions.
    peer_addresses: PeerAddresses,
    /// What this device last told each session about its endpoint, so a cloud that answers
    /// with a different record does not make it announce the same thing over and over.
    pushed_endpoints: HashMap<String, (String, Option<String>)>,
    friends: FriendLists,
    friends_dirty: bool,
    friends_sent: Instant,
    /// When the `feed` event was last sent.
    feed_sent: Instant,
    feed_dirty: bool,
    friends_answers: Answers,
    party: Option<anyplay_api::Party>,
    party_answers: Answers,
    party_invitations: HashMap<String, anyplay_api::PartyInvitation>,
    voice: VoiceSide,
    screen: ScreenSide,
    share: share::ShareSide,
    conversation_answers: Answers,
    /// The pace of typing signals to the cloud (see [`crate::typing`]).
    typing: crate::typing::TypingThrottle,
    /// The mutes that limit the account, as the launcher was last told.
    restrictions: Vec<control::Restriction>,
    discord: discord::DiscordSide,
    restriction_answers: Answers,
    /// When the restrictions were last asked of the cloud, for the web API answers.
    restrictions_asked: Option<Instant>,
    sessions: HashMap<String, Session>,
    /// Per session, so that a session read on going online never undoes a newer push.
    session_answers: HashMap<String, Answers>,
    invitations: HashMap<String, Invitation>,
    activity: Option<Activity>,
    game: Option<GameRun>,
    bridge: Option<BridgeLink>,
    bridge_port: u16,
    /// The token of the running game, which the bridge listener checks bridges against.
    bridge_tokens: crate::bridge::Tokens,
    dialogs: HashMap<u64, Dialog>,
    next_dialog: u64,
    traffic: Traffic,
    secret: SecretKey,
    tunnels: Option<Tunnels>,
    /// Counts the tunnels started, so a report of earlier ones is dropped.
    tunnels_generation: u64,
    /// What the last net report of the running tunnels found.
    reach: Option<tunnel::Reach>,
    tunnels_starting: bool,
    relays_expire: u64,
    relays_asked: Option<Instant>,
    links: HashMap<u64, Link>,
    link_totals: LinkTotals,
    observer: Observer,
    last_state: Option<(control::Cloud, Option<SignedIn>, Option<Pending>)>,
    last_bridge_state: Option<bridge::State>,
    control_closed: bool,
}

/// Something that happens once: [`Once::first`] is true the first time only.
#[derive(Default)]
struct Once(bool);

impl Once {
    fn first(&mut self) -> bool {
        !std::mem::replace(&mut self.0, true)
    }
}

/// Runs the service until the launcher's input ends and no game holds it, or until `stop`
/// completes.
pub async fn run<R, W>(
    config: Config,
    input: R,
    output: W,
    stop: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (service, inbox) = Service::new(config).await?;
    let stopper = service.inbox.clone();
    tokio::spawn(async move {
        stop.await;
        let _ = stopper.send(Input::Stop);
    });
    service.start(input, output, inbox).await;
    Ok(())
}

impl Service {
    async fn new(config: Config) -> std::io::Result<(Self, UnboundedReceiver<Input>)> {
        let (tx, rx) = mpsc::unbounded_channel();
        let (bridge_tx, mut bridge_rx) = mpsc::unbounded_channel();
        let bridge_tokens = crate::bridge::Tokens::default();
        let bridge_port = crate::bridge::listen(bridge_tx, bridge_tokens.clone()).await?;
        {
            let tx = tx.clone();
            tokio::spawn(async move {
                while let Some(input) = bridge_rx.recv().await {
                    if tx.send(Input::Bridge(input)).is_err() {
                        return;
                    }
                }
            });
        }
        let traffic = Traffic::start().await?;
        let secret = endpoint_key(&config.data);
        let observer = Observer::start(config.data.join("observations"));
        let (out, _) = mpsc::unbounded_channel();
        let (discord_tx, mut discord_rx) = mpsc::unbounded_channel();
        let discord = discord::DiscordSide::new(config.discord.open(discord_tx));
        {
            let tx = tx.clone();
            tokio::spawn(async move {
                while let Some(event) = discord_rx.recv().await {
                    if tx.send(Input::Discord(event)).is_err() {
                        return;
                    }
                }
            });
        }
        let service = Self {
            config,
            inbox: tx,
            out,
            hello: false,
            cloud: None,
            store: None,
            me: None,
            pending: None,
            browser: None,
            link: control::Cloud::Offline,
            offline_since: None,
            client_too_old: Once::default(),
            events: None,
            next_events: 1,
            address: None,
            peer_addresses: PeerAddresses::default(),
            pushed_endpoints: HashMap::new(),
            friends: FriendLists::default(),
            friends_dirty: false,
            friends_sent: Instant::now() - FRIENDS_PACE,
            feed_sent: Instant::now() - FEED_PACE,
            feed_dirty: false,
            friends_answers: Answers::default(),
            party: None,
            party_answers: Answers::default(),
            party_invitations: HashMap::new(),
            voice: VoiceSide::default(),
            screen: ScreenSide::default(),
            share: share::ShareSide::default(),
            conversation_answers: Answers::default(),
            typing: crate::typing::TypingThrottle::default(),
            restrictions: Vec::new(),
            discord,
            restriction_answers: Answers::default(),
            restrictions_asked: None,
            sessions: HashMap::new(),
            session_answers: HashMap::new(),
            invitations: HashMap::new(),
            activity: None,
            game: None,
            bridge: None,
            bridge_port,
            bridge_tokens,
            dialogs: HashMap::new(),
            next_dialog: 1,
            traffic,
            secret,
            tunnels: None,
            tunnels_generation: 0,
            reach: None,
            tunnels_starting: false,
            relays_expire: 0,
            relays_asked: None,
            links: HashMap::new(),
            link_totals: LinkTotals::default(),
            observer,
            last_state: None,
            last_bridge_state: None,
            control_closed: false,
        };
        Ok((service, rx))
    }

    async fn start<R, W>(mut self, input: R, output: W, mut inbox: UnboundedReceiver<Input>)
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (out, lines) = mpsc::unbounded_channel();
        self.out = out;
        let writer = tokio::spawn(write_lines(output, lines));
        tokio::spawn(read_commands(input, self.inbox.clone()));
        {
            let tx = self.inbox.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(FRIENDS_PACE);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tick.tick().await;
                    if tx.send(Input::Tick).is_err() {
                        return;
                    }
                }
            });
        }
        while let Some(input) = inbox.recv().await {
            match input {
                Input::Command(id, command) => self.command(id, command),
                Input::BadCommand(Some(id)) => self.answer(Answer::error(
                    id,
                    Failure::new(Failure::UNKNOWN_COMMAND, "This command is not known."),
                )),
                Input::BadCommand(None) => tracing::warn!("unreadable line from the launcher"),
                Input::ControlClosed => {
                    self.control_closed = true;
                    if self.bridge.is_none() {
                        break;
                    }
                    tracing::info!("the launcher left; staying until the game ends");
                }
                Input::Events(generation, input) => {
                    if self.events.as_ref().is_some_and(|(g, _)| *g == generation) {
                        self.cloud_event(input);
                    }
                }
                Input::Bridge(input) => {
                    if self.bridge_input(input) {
                        break;
                    }
                }
                Input::Link(change) => self.link_change(change),
                Input::Reach(generation, reach) => {
                    if generation == self.tunnels_generation
                        && self.tunnels.is_some()
                        && self.reach.as_ref() != Some(&reach)
                    {
                        tracing::info!("net report: {reach:?}");
                        self.reach = Some(reach);
                        self.bridge_state();
                    }
                }
                Input::Voice(generation, report) => {
                    if generation == self.voice.generation && self.voice.hub.is_some() {
                        self.voice.report = Some(report);
                        self.voice_event();
                    }
                }
                Input::Screen(generation, output) => {
                    if generation == self.screen.generation && self.screen.hub.is_some() {
                        self.screen_output(output);
                    }
                }
                Input::Play(generation, output) => {
                    if self.share.hub_is_there(generation) {
                        self.share_output(output);
                    }
                }
                Input::Discord(event) => self.discord_event(event),
                Input::Done(continuation) => continuation(&mut self),
                Input::Tick => self.tick(),
                Input::Stop => break,
            }
        }
        self.goodbye().await;
        drop(self.out);
        let _ = writer.await;
    }

    // Plumbing

    fn spawn<T: Send + 'static>(
        &self,
        work: impl Future<Output = T> + Send + 'static,
        then: impl FnOnce(&mut Service, T) + Send + 'static,
    ) {
        let inbox = self.inbox.clone();
        tokio::spawn(async move {
            let value = work.await;
            let _ = inbox.send(Input::Done(Box::new(move |service| then(service, value))));
        });
    }

    fn send(&self, output: Output) {
        let _ = self.out.send(control::output_line(&output));
    }

    fn answer(&self, answer: Answer) {
        self.send(Output::Answer(answer));
    }

    fn ok(&self, id: u64, value: impl serde::Serialize) {
        self.answer(Answer::ok(id, value));
    }

    fn fail(&self, id: u64, failure: Failure) {
        self.answer(Answer::error(id, failure));
    }

    fn event(&self, event: control::Event) {
        self.send(Output::Event(event));
    }

    fn cloud(&self, id: u64) -> Option<Cloud> {
        match &self.cloud {
            Some(cloud) => Some(cloud.clone()),
            None => {
                self.fail(
                    id,
                    Failure::new(Failure::BAD_STATE, "Say hello with a cloud first."),
                );
                None
            }
        }
    }

    fn signed_in_cloud(&self, id: u64) -> Option<Cloud> {
        let cloud = self.cloud(id)?;
        if self.me.is_none() {
            self.fail(id, Failure::new(Failure::NOT_SIGNED_IN, "Sign in first."));
            return None;
        }
        Some(cloud)
    }

    /// Runs a cloud request for command `id` and answers it with `done`'s value or the error.
    fn request<T: Send + 'static, A: serde::Serialize>(
        &self,
        id: u64,
        work: impl Future<Output = Result<T, CloudError>> + Send + 'static,
        done: impl FnOnce(&mut Service, T) -> A + Send + 'static,
    ) {
        self.spawn(work, move |service, result| match result {
            Ok(value) => {
                let answer = done(service, value);
                service.ok(id, answer);
            }
            Err(error) => {
                let failure = service.failure(&error);
                service.fail(id, failure);
            }
        });
    }

    fn failure(&mut self, error: &CloudError) -> Failure {
        match error {
            CloudError::Api(api) => {
                let failure = Failure::api(api);
                match api.error {
                    ErrorCode::Banned => {
                        if let Some(restriction) = failure.restriction.clone() {
                            self.event(control::Event::Banned {
                                restriction: *restriction,
                            });
                        }
                        self.end_online("banned");
                    }
                    ErrorCode::ClientTooOld => {
                        self.client_too_old(api.minimum.as_deref().unwrap_or_default().to_owned());
                    }
                    // A mute the launcher was not told of, such as one that began while it was
                    // away.
                    ErrorCode::Restricted => self.fetch_restrictions(),
                    _ => {}
                }
                failure
            }
            CloudError::Offline(reason) => Failure::new(Failure::OFFLINE, reason.clone()),
            CloudError::SignedOut => {
                self.end_online("signed out");
                Failure::new(
                    Failure::NOT_SIGNED_IN,
                    "This device is no longer signed in.",
                )
            }
        }
    }

    fn device(&self) -> Device {
        Device {
            name: self.config.device_name.clone(),
            platform: Platform::current(),
            key: None,
        }
    }

    // Commands

    fn command(&mut self, id: u64, command: Command) {
        if !self.hello && !matches!(command, Command::Hello { .. }) {
            self.fail(id, Failure::new(Failure::BAD_STATE, "Say hello first."));
            return;
        }
        match command {
            Command::Hello { launcher, cloud } => {
                self.hello(id, &launcher, &cloud);
                self.discord_changed();
            }
            Command::DiscordStatus => self.discord_status(id),
            Command::DiscordConnect => self.discord_connect(id),
            Command::DiscordDisconnect => self.discord_disconnect(id),
            Command::Email { email } => {
                let Some(cloud) = self.cloud(id) else { return };
                if self.me.is_some() {
                    self.fail(id, Failure::new(Failure::BAD_STATE, "Already signed in."));
                    return;
                }
                self.request(
                    id,
                    async move { cloud.email(&email).await },
                    |service, answer| {
                        service.pending = Some(PendingSignIn::Code {
                            challenge: answer.challenge,
                        });
                        service.state_changed();
                        EmailSent {
                            code_length: answer.code_length,
                        }
                    },
                );
            }
            Command::Code { code } => {
                let Some(cloud) = self.cloud(id) else { return };
                let Some(PendingSignIn::Code { challenge }) = &self.pending else {
                    self.fail(
                        id,
                        Failure::new(Failure::BAD_STATE, "Ask for a code first."),
                    );
                    return;
                };
                let challenge = challenge.clone();
                let device = self.device();
                self.request(
                    id,
                    async move { cloud.code(&challenge, code.trim(), device).await },
                    |service, answer| match answer {
                        CodeAnswer::SignedIn { me, .. } => {
                            service.pending = None;
                            service.signed_in(me);
                            CodeAccepted {
                                result: CodeResult::SignedIn,
                            }
                        }
                        CodeAnswer::NewAccount { signup, .. } => {
                            service.pending = Some(PendingSignIn::Handle { signup });
                            service.state_changed();
                            CodeAccepted {
                                result: CodeResult::NewAccount,
                            }
                        }
                    },
                );
            }
            Command::SignUp {
                handle,
                age_confirmed,
            } => {
                let Some(cloud) = self.cloud(id) else { return };
                let Some(PendingSignIn::Handle { signup }) = &self.pending else {
                    self.fail(id, Failure::new(Failure::BAD_STATE, "Enter a code first."));
                    return;
                };
                if !age_confirmed {
                    self.fail(
                        id,
                        Failure::new(
                            ErrorCode::BadRequest.as_str(),
                            "The player must confirm being old enough.",
                        ),
                    );
                    return;
                }
                if check_handle(&handle).is_err() {
                    self.fail(
                        id,
                        Failure::new(
                            ErrorCode::HandleInvalid.as_str(),
                            "A handle has 3 to 16 letters, digits, - or _, and starts with a letter.",
                        ),
                    );
                    return;
                }
                let signup = signup.clone();
                let device = self.device();
                self.request(
                    id,
                    async move { cloud.signup(&signup, &handle, device).await },
                    |service, answer| {
                        service.pending = None;
                        service.signed_in(answer.me);
                        Done {}
                    },
                );
            }
            Command::SignOut => {
                let Some(cloud) = self.cloud(id) else { return };
                let sessions: Vec<String> = self.sessions.keys().cloned().collect();
                self.spawn(
                    async move {
                        for session in sessions {
                            let _ = cloud.leave(&session).await;
                        }
                        cloud.signout().await
                    },
                    move |service, _| {
                        service.discord_end(true);
                        service.end_online("signed out");
                        service.ok(id, Done {});
                    },
                );
            }
            Command::DeleteAccount => {
                let Some(cloud) = self.signed_in_cloud(id) else {
                    return;
                };
                self.request(id, async move { cloud.delete_me().await }, |service, ()| {
                    service.discord_end(true);
                    service.end_online("account deleted");
                    Done {}
                });
            }
            Command::AddFriend { handle } => {
                let Some(cloud) = self.signed_in_cloud(id) else {
                    return;
                };
                self.request(
                    id,
                    async move { cloud.add_friend(handle.trim()).await },
                    |service, answer| {
                        service.fetch_friends();
                        FriendAdded {
                            result: answer.state,
                        }
                    },
                );
            }
            Command::Accept { account } => {
                self.friend_change(id, move |c| async move { c.accept(account).await })
            }
            Command::Decline { account } => {
                self.friend_change(id, move |c| async move { c.decline(account).await })
            }
            Command::RemoveFriend { account } => {
                self.friend_change(id, move |c| async move { c.remove_friend(account).await })
            }
            Command::Block { account } => {
                self.friend_change(id, move |c| async move { c.block(account).await })
            }
            Command::Unblock { account } => {
                self.friend_change(id, move |c| async move { c.unblock(account).await })
            }
            Command::Report {
                account,
                reason,
                details,
                conversation,
                seq,
                listing,
            } => {
                if conversation.as_deref().is_some_and(|c| !is_title_id(c))
                    || conversation.is_some() != seq.is_some()
                    || listing.as_deref().is_some_and(|l| !is_title_id(l))
                {
                    self.fail(id, bad_id());
                    return;
                }
                let Some(cloud) = self.signed_in_cloud(id) else {
                    return;
                };
                let details: String = details
                    .chars()
                    .take(anyplay_api::MAX_REPORT_DETAILS)
                    .collect();
                self.request(
                    id,
                    async move {
                        cloud
                            .report(&Report {
                                account,
                                reason,
                                details,
                                conversation,
                                seq,
                                listing,
                            })
                            .await
                    },
                    |_, ()| Done {},
                );
            }
            Command::UploadTrophies { title_id, trophies } => {
                if !is_title_id(&title_id) {
                    self.fail(id, bad_title());
                    return;
                }
                let Some(cloud) = self.signed_in_cloud(id) else {
                    return;
                };
                self.request(
                    id,
                    async move { cloud.upload_trophies(&title_id, trophies).await },
                    |_, added| added,
                );
            }
            Command::Trophies { account } => {
                let Some(cloud) = self.signed_in_cloud(id) else {
                    return;
                };
                self.request(
                    id,
                    async move { cloud.trophies(account).await },
                    |_, trophies| trophies,
                );
            }
            Command::Rarity { title_id } => {
                if !is_title_id(&title_id) {
                    self.fail(id, bad_title());
                    return;
                }
                let Some(cloud) = self.signed_in_cloud(id) else {
                    return;
                };
                self.request(
                    id,
                    async move { cloud.rarity(&title_id).await },
                    |_, rarity| rarity,
                );
            }
            Command::Compare { account, title_id } => {
                if !is_title_id(&title_id) {
                    self.fail(id, bad_title());
                    return;
                }
                let Some(cloud) = self.signed_in_cloud(id) else {
                    return;
                };
                self.request(
                    id,
                    async move { cloud.compare(account, &title_id).await },
                    |_, compare| compare,
                );
            }
            Command::SecretTrophies { title_id, numbers } => {
                if !is_title_id(&title_id) {
                    self.fail(id, bad_title());
                    return;
                }
                let Some(cloud) = self.signed_in_cloud(id) else {
                    return;
                };
                self.request(
                    id,
                    async move { cloud.set_secret(&title_id, numbers, false).await },
                    |_, changed| changed,
                );
            }
            Command::RevealTrophies { title_id, numbers } => {
                if !is_title_id(&title_id) {
                    self.fail(id, bad_title());
                    return;
                }
                let Some(cloud) = self.signed_in_cloud(id) else {
                    return;
                };
                self.request(
                    id,
                    async move { cloud.set_secret(&title_id, numbers, true).await },
                    |_, changed| changed,
                );
            }
            Command::RunReport {
                title_id,
                version,
                toolkit,
                verdict,
            } => {
                if !is_title_id(&title_id) {
                    self.fail(id, bad_title());
                    return;
                }
                if !anyplay_api::is_run_version(&version) || !anyplay_api::is_run_version(&toolkit)
                {
                    self.fail(id, bad_run_version());
                    return;
                }
                let Some(os) = this_os() else {
                    self.fail(
                        id,
                        Failure::new(
                            ErrorCode::BadRequest.as_str(),
                            "Run reports come from Windows, Linux and macOS.",
                        ),
                    );
                    return;
                };
                let Some(cloud) = self.signed_in_cloud(id) else {
                    return;
                };
                let report = anyplay_api::RunReport {
                    version,
                    toolkit,
                    verdict,
                    os,
                };
                self.request(
                    id,
                    async move { cloud.run_report(&title_id, &report).await },
                    |_, ()| Done {},
                );
            }
            Command::RunReports { title_id, version } => {
                if !is_title_id(&title_id) {
                    self.fail(id, bad_title());
                    return;
                }
                if version
                    .as_deref()
                    .is_some_and(|v| !anyplay_api::is_run_version(v))
                {
                    self.fail(id, bad_run_version());
                    return;
                }
                let Some(cloud) = self.signed_in_cloud(id) else {
                    return;
                };
                self.request(
                    id,
                    async move { cloud.run_summary(&title_id, version.as_deref()).await },
                    |_, summary| summary,
                );
            }
            Command::Cards { accounts } => {
                let Some(cloud) = self.signed_in_cloud(id) else {
                    return;
                };
                self.request(
                    id,
                    async move { cloud.cards(&accounts).await },
                    |_, cards| cards,
                );
            }
            Command::PlayedWith => {
                let Some(cloud) = self.signed_in_cloud(id) else {
                    return;
                };
                self.request(id, async move { cloud.played_with().await }, |_, l| l);
            }
            Command::ClearPlayedWith => {
                let Some(cloud) = self.signed_in_cloud(id) else {
                    return;
                };
                self.request(
                    id,
                    async move { cloud.clear_played_with().await },
                    |_, ()| Done {},
                );
            }
            Command::SetRememberPlayedWith { on } => {
                let Some(cloud) = self.signed_in_cloud(id) else {
                    return;
                };
                self.request(
                    id,
                    async move { cloud.set_remember_played_with(on).await },
                    |_, r| r,
                );
            }
            Command::PublicProfile => {
                let Some(cloud) = self.signed_in_cloud(id) else {
                    return;
                };
                self.request(id, async move { cloud.public_profile().await }, |_, p| p);
            }
            Command::SetPublicProfile { on } => {
                let Some(cloud) = self.signed_in_cloud(id) else {
                    return;
                };
                self.request(
                    id,
                    async move { cloud.set_public_profile(on).await },
                    |_, p| p,
                );
            }
            Command::Privacy => {
                let Some(cloud) = self.signed_in_cloud(id) else {
                    return;
                };
                self.request(id, async move { cloud.privacy().await }, |_, p| p);
            }
            Command::SetPrivacy { privacy } => {
                let Some(cloud) = self.signed_in_cloud(id) else {
                    return;
                };
                self.request(
                    id,
                    async move { cloud.set_privacy(&privacy).await },
                    |_, p| p,
                );
            }
            Command::Profile { account } => {
                let Some(cloud) = self.signed_in_cloud(id) else {
                    return;
                };
                self.request(
                    id,
                    async move { cloud.profile(account).await },
                    |_, profile| profile,
                );
            }
            Command::Feed { before, limit } => {
                let Some(cloud) = self.signed_in_cloud(id) else {
                    return;
                };
                self.request(
                    id,
                    async move { cloud.feed(before, limit).await },
                    |_, feed| feed,
                );
            }
            Command::React { item, reaction } => {
                let Some(cloud) = self.signed_in_cloud(id) else {
                    return;
                };
                self.request(
                    id,
                    async move { cloud.react(item, reaction).await },
                    |_, reactions| reactions,
                );
            }
            Command::UploadPlaytime { entries } => {
                if let Some(problem) = playtime_problem(&entries) {
                    self.fail(id, Failure::new(ErrorCode::BadRequest.as_str(), problem));
                    return;
                }
                let Some(cloud) = self.signed_in_cloud(id) else {
                    return;
                };
                self.request(
                    id,
                    async move { cloud.upload_playtime(entries).await },
                    |_, changed| changed,
                );
            }
            Command::BrowserSignIn { create } => {
                let Some(cloud) = self.cloud(id) else { return };
                if self.me.is_some() {
                    self.fail(id, Failure::new(Failure::BAD_STATE, "Already signed in."));
                    return;
                }
                self.stop_browser();
                let name = self.config.device_name.clone();
                self.spawn(
                    async move {
                        let info = cloud.info().await?;
                        let Some(web) = info.web else {
                            return Ok(None);
                        };
                        browser::start(&web, create, &name)
                            .await
                            .map(Some)
                            .map_err(|e| CloudError::Offline(e.to_string()))
                    },
                    move |service, result| match result {
                        Ok(Some(started)) => {
                            service.pending = Some(PendingSignIn::Browser);
                            service.state_changed();
                            service.ok(
                                id,
                                Opened {
                                    url: started.url.clone(),
                                },
                            );
                            service.await_browser(started);
                        }
                        Ok(None) => service.fail(
                            id,
                            Failure::new(
                                ErrorCode::NotFound.as_str(),
                                "This cloud has no account web. Sign in with an e-mail code.",
                            ),
                        ),
                        Err(error) => {
                            let failure = service.failure(&error);
                            service.fail(id, failure);
                        }
                    },
                );
            }
            Command::CancelSignIn => {
                if matches!(self.pending, Some(PendingSignIn::Browser)) {
                    self.stop_browser();
                    self.pending = None;
                    self.state_changed();
                }
                self.ok(id, Done {});
            }
            Command::AccountPage => {
                let Some(cloud) = self.signed_in_cloud(id) else {
                    return;
                };
                self.request(
                    id,
                    async move {
                        let link = cloud.account_link().await?;
                        if browser::is_web_url(&link.url) {
                            Ok(link)
                        } else {
                            Err(CloudError::Offline(
                                "the cloud sent a page address that is not http or https".into(),
                            ))
                        }
                    },
                    |_, link| Opened { url: link.url },
                );
            }
            Command::Appeal { action, text } => {
                let Some(cloud) = self.signed_in_cloud(id) else {
                    return;
                };
                self.request(
                    id,
                    async move { cloud.appeal(action, &text).await },
                    |service, _| {
                        service.fetch_restrictions();
                        Done {}
                    },
                );
            }
            Command::RelayOnly { on } => {
                if self.config.relay_only != on {
                    self.config.relay_only = on;
                    self.restart_tunnels();
                }
                self.ok(id, Done {});
            }
            Command::SetAppearOffline { on } => {
                let Some(cloud) = self.signed_in_cloud(id) else {
                    return;
                };
                self.request(
                    id,
                    async move { cloud.set_appear_offline(on).await },
                    |service, answer| {
                        service.set_appear_offline(answer.appear_offline);
                        answer
                    },
                );
            }
            Command::Playing { game } => {
                self.activity = game.map(|game| Activity {
                    title_id: game.title_id,
                    title: game.title,
                    version: game.version,
                    session: None,
                });
                self.push_presence();
                self.ok(id, Done {});
            }
            Command::CheckJoin {
                session,
                title_id,
                version,
                build,
                version_rule,
                override_version,
            } => {
                let Some(cloud) = self.signed_in_cloud(id) else {
                    return;
                };
                self.spawn(
                    async move { cloud.session(&session).await },
                    move |service, result| match result {
                        Ok(found) => {
                            if found.title_id != title_id {
                                service.fail(
                                    id,
                                    Failure::new(
                                        ErrorCode::NotFound.as_str(),
                                        "The session belongs to another game.",
                                    ),
                                );
                            } else if !(override_version
                                || version_rule.allows(&found.version, &version))
                                || found.build != build
                            {
                                let mut failure = Failure::new(
                                    ErrorCode::VersionMismatch.as_str(),
                                    format!(
                                        "Your friend plays version {} with AnyPS5 {}.",
                                        found.version, found.build
                                    ),
                                );
                                failure.expected = Some(Box::new(anyplay_api::Expected {
                                    version: found.version,
                                    build: found.build,
                                }));
                                service.fail(id, failure);
                            } else {
                                service.ok(id, Done {});
                            }
                        }
                        Err(error) => {
                            let failure = service.failure(&error);
                            service.fail(id, failure);
                        }
                    },
                );
            }
            Command::GameStart {
                title_id,
                title,
                version,
                build,
                join,
                host,
                version_rule,
                override_version,
            } => self.game_start(
                id,
                title_id,
                title,
                version,
                build,
                join,
                host,
                (version_rule, override_version),
            ),
            Command::GameEnd => {
                self.game_end();
                self.ok(id, Done {});
            }
            Command::Invite { accounts } => {
                let Some(cloud) = self.signed_in_cloud(id) else {
                    return;
                };
                let Some(session) = self.current_session() else {
                    self.fail(
                        id,
                        Failure::new(Failure::NO_GAME, "You are not in a session."),
                    );
                    return;
                };
                self.request(
                    id,
                    async move { cloud.invite(&session, accounts).await },
                    |_, invited| Invited { sent: invited.sent },
                );
            }
            Command::AnswerInvitation {
                id: invitation,
                accept,
            } => {
                let Some(cloud) = self.signed_in_cloud(id) else {
                    return;
                };
                let Some(found) = self.invitations.get(&invitation).cloned() else {
                    self.fail(
                        id,
                        Failure::new(ErrorCode::NotFound.as_str(), "The invitation is gone."),
                    );
                    return;
                };
                if accept {
                    self.ok(
                        id,
                        Accepted {
                            session: found.session,
                            title_id: found.title_id,
                        },
                    );
                } else {
                    self.invitations.remove(&invitation);
                    self.event(control::Event::InvitationGone {
                        id: invitation.clone(),
                    });
                    self.request(
                        id,
                        async move { cloud.decline_invitation(&invitation).await },
                        |_, ()| Done {},
                    );
                }
            }
            Command::Dialog {
                id: dialog,
                accounts,
            } => {
                let Some(pending) = self.dialogs.remove(&dialog) else {
                    self.fail(id, Failure::new(Failure::BAD_STATE, "No such dialog."));
                    return;
                };
                self.ok(id, Done {});
                let accounts = accounts.unwrap_or_default();
                let Some(cloud) = self.cloud.clone().filter(|_| !accounts.is_empty()) else {
                    self.bridge_reply(
                        pending.connection,
                        pending.request,
                        Message::InvitationDialogReply {
                            result: DialogResult::Cancelled,
                            sent: 0,
                        },
                    );
                    return;
                };
                let session = pending.session.clone();
                self.spawn(
                    async move { cloud.invite(&session, accounts).await },
                    move |service, result| {
                        let reply = match result {
                            Ok(invited) => Message::InvitationDialogReply {
                                result: DialogResult::Sent,
                                sent: invited.sent.len().min(u16::MAX as usize) as u16,
                            },
                            Err(_) => Message::InvitationDialogReply {
                                result: DialogResult::Cancelled,
                                sent: 0,
                            },
                        };
                        service.bridge_reply(pending.connection, pending.request, reply);
                    },
                );
            }
            // Share screen
            Command::ScreenStart {
                source,
                window,
                pids,
            } => self.screen_start(id, source, window, pids),
            Command::ScreenStop => {
                if let Some(hub) = &self.screen.hub {
                    hub.stop_sharing("ended");
                }
                self.ok(id, Done {});
            }
            Command::ScreenWatch { account } => self.screen_watch(id, account),
            Command::ScreenLeave => {
                if let Some(hub) = &self.screen.hub {
                    hub.leave();
                }
                self.ok(id, Done {});
            }
            Command::ScreenWorker { path } => match self.config.workers.set_path(&path) {
                Ok(capabilities) => {
                    let codecs = |list: Vec<anyplay_share::control::CodecCaps>| {
                        list.into_iter()
                            .map(|c| control::ScreenCodec {
                                codec: c.codec,
                                hardware: c.hardware,
                                max_width: c.max_width,
                                max_height: c.max_height,
                                max_fps: c.max_fps,
                            })
                            .collect()
                    };
                    self.screen_event();
                    self.share_changed();
                    self.ok(
                        id,
                        control::ScreenWorkerReady {
                            encoders: codecs(capabilities.encode),
                            decoders: codecs(capabilities.decode),
                            capture: capabilities.capture,
                            libraries: capabilities
                                .libraries
                                .into_iter()
                                .map(|library| control::ScreenLibrary {
                                    name: library.name,
                                    state: library.state.as_str().into(),
                                    detail: library.detail,
                                })
                                .collect(),
                        },
                    );
                }
                Err(error) => self.fail(id, Failure::new(ErrorCode::NotFound.as_str(), error)),
            },
            // Share play
            Command::ShareStart { account, mode } => self.share_start(id, account, mode),
            Command::ShareAnswer {
                id: invitation,
                accept,
            } => self.share_answer(id, invitation, accept),
            Command::ShareMode { mode } => self.share_mode(id, mode),
            Command::ShareRequest { mode } => self.share_request(id, mode),
            Command::ShareQuality {
                max_kbps,
                max_height,
            } => self.share_quality(id, max_kbps, max_height),
            Command::ShareStop => self.share_stop(id),
            // Voice
            Command::Voice { on } => {
                self.voice.enabled = on;
                self.voice_sync();
                self.voice_event();
                self.ok(id, Done {});
            }
            Command::PushToTalk { talking } => {
                self.voice.settings.talking = talking;
                if let Some(hub) = &self.voice.hub {
                    hub.set_talking(talking);
                }
                self.ok(id, Done {});
            }
            Command::VoiceMode { mode } => {
                let mode = match mode {
                    control::VoiceMode::PushToTalk => anyplay_voice::TalkMode::PushToTalk,
                    control::VoiceMode::VoiceActivity => anyplay_voice::TalkMode::VoiceActivity,
                };
                self.voice.settings.mode = mode;
                if let Some(hub) = &self.voice.hub {
                    hub.set_mode(mode);
                }
                self.voice_event();
                self.ok(id, Done {});
            }
            Command::VoiceNoise { enabled } => {
                self.voice.settings.noise_suppression = Some(enabled);
                if let Some(hub) = &self.voice.hub {
                    hub.set_noise_suppression(enabled);
                }
                self.ok(id, Done {});
            }
            Command::VoiceEcho { enabled } => {
                self.voice.settings.echo_cancellation = Some(enabled);
                if let Some(hub) = &self.voice.hub {
                    hub.set_echo_cancellation(enabled);
                }
                self.ok(id, Done {});
            }
            Command::MicMute { muted } => {
                self.voice.settings.mic_muted = muted;
                if let Some(hub) = &self.voice.hub {
                    hub.set_mic_muted(muted);
                }
                self.voice_event();
                self.ok(id, Done {});
            }
            Command::Deafen { deafened } => {
                self.voice.settings.deafened = deafened;
                if let Some(hub) = &self.voice.hub {
                    hub.set_deafened(deafened);
                }
                self.voice_event();
                self.ok(id, Done {});
            }
            Command::PeerVolume { account, percent } => {
                if percent > MAX_PEER_VOLUME_PERCENT {
                    self.fail(
                        id,
                        Failure::new(
                            ErrorCode::BadRequest.as_str(),
                            "A volume is 0 to 200 percent.",
                        ),
                    );
                    return;
                }
                let volume = f32::from(percent) / 100.0;
                self.voice.settings.volumes.insert(account.0, volume);
                if let Some(hub) = &self.voice.hub {
                    hub.set_volume(account.0, volume);
                }
                self.ok(id, Done {});
            }
            Command::PeerMute { account, muted } => {
                if muted {
                    self.voice.settings.muted.insert(account.0);
                } else {
                    self.voice.settings.muted.remove(&account.0);
                }
                if let Some(hub) = &self.voice.hub {
                    hub.set_muted(account.0, muted);
                }
                self.ok(id, Done {});
            }
            Command::VoiceDevices => {
                let audio = self.config.audio.clone();
                self.spawn(
                    async move { list_devices(audio).await },
                    move |service, (input, output)| {
                        service.ok(
                            id,
                            control::VoiceDevices {
                                input,
                                output,
                                selected_input: service.voice.input.clone(),
                                selected_output: service.voice.output.clone(),
                            },
                        );
                    },
                );
            }
            Command::SetVoiceDevice { kind, name } => {
                let audio = self.config.audio.clone();
                self.spawn(
                    async move { list_devices(audio).await },
                    move |service, (input, output)| {
                        let known = match kind {
                            control::DeviceKind::Input => input,
                            control::DeviceKind::Output => output,
                        };
                        if let Some(name) = &name
                            && !known.contains(name)
                        {
                            service.fail(
                                id,
                                Failure::new(
                                    ErrorCode::NotFound.as_str(),
                                    "No sound device has this name.",
                                ),
                            );
                            return;
                        }
                        match kind {
                            control::DeviceKind::Input => service.voice.input = name,
                            control::DeviceKind::Output => service.voice.output = name,
                        }
                        service.voice_restart();
                        service.ok(id, Done {});
                    },
                );
            }
            // Parties and messages
            social @ (Command::Party
            | Command::PartyCreate
            | Command::PartyInvite { .. }
            | Command::MissedCalls
            | Command::CallsSeen { .. }
            | Command::PartyLink { .. }
            | Command::PartyAnswer { .. }
            | Command::PartyLeave
            | Command::PartyKick { .. }
            | Command::PartyLeader { .. }
            | Command::PartyMove { .. }
            | Command::JoinCode
            | Command::JoinCodeRevoke
            | Command::JoinRedeem { .. }
            | Command::Conversations
            | Command::OpenConversation { .. }
            | Command::Messages { .. }
            | Command::SendMessage { .. }
            | Command::EditMessage { .. }
            | Command::UnsendMessage { .. }
            | Command::ReactMessage { .. }
            | Command::Typing { .. }
            | Command::ConversationSettings { .. }
            | Command::MarkRead { .. }
            | Command::AcceptRequest { .. }
            | Command::LeaveConversation { .. }
            | Command::AddToConversation { .. }
            | Command::SendMedia { .. }
            | Command::FetchMedia { .. }
            | Command::Lfg { .. }
            | Command::LfgMine
            | Command::LfgPost { .. }
            | Command::LfgClose { .. }
            | Command::LfgAsk { .. }
            | Command::LfgWithdraw { .. }
            | Command::LfgAnswer { .. }) => self.social_command(id, social),
        }
    }

    fn friend_change<F>(&self, id: u64, work: impl FnOnce(Cloud) -> F)
    where
        F: Future<Output = Result<(), CloudError>> + Send + 'static,
    {
        let Some(cloud) = self.signed_in_cloud(id) else {
            return;
        };
        self.request(id, work(cloud), |service, ()| {
            service.fetch_friends();
            Done {}
        });
    }

    fn hello(&mut self, id: u64, launcher: &str, url: &str) {
        if self.hello {
            self.fail(
                id,
                Failure::new(Failure::BAD_STATE, "Hello was already said."),
            );
            return;
        }
        let url = url.trim().trim_end_matches('/');
        if !(url.starts_with("https://") || url.starts_with("http://")) || url.len() > 2048 {
            self.fail(
                id,
                Failure::new(
                    ErrorCode::BadRequest.as_str(),
                    "The cloud address must be an http or https URL.",
                ),
            );
            return;
        }
        tracing::info!("launcher {launcher}, cloud {url}");
        let store = Store::open(&self.config.data, url, self.config.file_credentials);
        let queue = crate::credentials::SaveQueue::new(store.clone());
        let sink: crate::cloud::TokenSink = Arc::new(move |tokens| {
            let saved = tokens.map(|(t, key)| Saved {
                device: t.device,
                refresh: t.refresh.clone(),
                key: key.map(anyplay_proof::DeviceKey::secret),
            });
            queue.save(saved);
        });
        let cloud = Cloud::new(url, sink);
        self.hello = true;
        self.cloud = Some(cloud.clone());
        self.ok(
            id,
            Hello {
                service: env!("CARGO_PKG_VERSION").to_owned(),
                protocol: control::VERSION,
            },
        );
        let loader = store.clone();
        self.store = Some(store);
        self.state_changed();
        self.spawn(
            async move {
                tokio::task::spawn_blocking(move || loader.load())
                    .await
                    .ok()
                    .flatten()
            },
            move |service, saved| {
                let Some(saved) = saved else { return };
                let key = saved
                    .key
                    .as_deref()
                    .and_then(anyplay_proof::DeviceKey::from_secret);
                cloud.resume(saved.device, saved.refresh, key);
                service.link = control::Cloud::Connecting;
                service.state_changed();
                let resumed = cloud.clone();
                service.spawn(
                    async move { resumed.me().await },
                    |service, result| match result {
                        Ok(me) => service.signed_in(me),
                        Err(CloudError::Offline(reason)) => {
                            tracing::info!("cloud unreachable, trying again: {reason}");
                            service.retry_resume();
                        }
                        Err(error) => {
                            tracing::info!("saved sign-in not accepted: {error}");
                            service.end_online("signed out");
                        }
                    },
                );
            },
        );
    }

    /// Waits for the browser to come back to the loopback address, then trades its code for
    /// the tokens of this device.
    fn await_browser(&mut self, started: browser::Started) {
        let browser::Started {
            verifier,
            result,
            task,
            ..
        } = started;
        let cloud = self.cloud.clone();
        let device = self.device();
        let inbox = self.inbox.clone();
        let waiter = tokio::spawn(async move {
            let outcome = result.await.unwrap_or(Callback::TimedOut);
            let Some(cloud) = cloud else { return };
            let exchanged = match &outcome {
                Callback::Code(code) => Some(cloud.browser_token(code, &verifier, device).await),
                _ => None,
            };
            let _ = inbox.send(Input::Done(Box::new(move |service| {
                service.browser_done(outcome, exchanged);
            })));
        });
        self.browser = Some((task, waiter));
    }

    fn stop_browser(&mut self) {
        if let Some((listener, waiter)) = self.browser.take() {
            listener.abort();
            waiter.abort();
        }
    }

    fn browser_done(
        &mut self,
        outcome: Callback,
        exchanged: Option<Result<anyplay_api::SignedIn, CloudError>>,
    ) {
        self.browser = None;
        self.pending = None;
        let notice = |code: &str, message: &str| control::Event::Notice {
            code: code.to_owned(),
            message: message.to_owned(),
        };
        match (outcome, exchanged) {
            (_, Some(Ok(signed_in))) => self.signed_in(signed_in.me),
            (_, Some(Err(error))) => {
                let failure = self.failure(&error);
                self.event(notice(&failure.code, &failure.message));
                self.state_changed();
            }
            (Callback::Denied(_), _) => {
                self.event(notice(
                    "sign_in_stopped",
                    "The sign-in was stopped in the browser.",
                ));
                self.state_changed();
            }
            _ => {
                self.event(notice(
                    "sign_in_timed_out",
                    "The sign-in took too long. Start it again.",
                ));
                self.state_changed();
            }
        }
    }

    /// Tries a saved sign-in again after a pause, while the cloud stays unreachable.
    fn retry_resume(&mut self) {
        let Some(cloud) = self.cloud.clone() else {
            return;
        };
        self.link = control::Cloud::Offline;
        self.state_changed();
        self.spawn(
            async move {
                tokio::time::sleep(Duration::from_secs(10)).await;
                cloud.me().await
            },
            |service, result| match result {
                Ok(me) => service.signed_in(me),
                Err(CloudError::Offline(_))
                    if service.cloud.as_ref().is_some_and(Cloud::is_signed_in) =>
                {
                    service.retry_resume();
                }
                Err(_) => service.end_online("signed out"),
            },
        );
    }

    // Signing in and out

    fn signed_in(&mut self, me: Me) {
        tracing::info!("signed in as {}", me.handle);
        self.me = Some(me);
        self.state_changed();
        self.start_events();
        self.fetch_friends();
        self.push_presence();
        self.ask_relays();
    }

    fn start_events(&mut self) {
        let Some(cloud) = self.cloud.clone() else {
            return;
        };
        if let Some((_, task)) = self.events.take() {
            task.abort();
        }
        let generation = self.next_events;
        self.next_events += 1;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let task = events::spawn(cloud, tx);
        let inbox = self.inbox.clone();
        tokio::spawn(async move {
            while let Some(input) = rx.recv().await {
                if inbox.send(Input::Events(generation, input)).is_err() {
                    return;
                }
            }
        });
        self.events = Some((generation, task));
        self.link = control::Cloud::Connecting;
    }

    /// Forgets everything that belongs to the signed-in account.
    fn end_online(&mut self, why: &str) {
        if self.me.is_none() && self.events.is_none() && self.pending.is_none() {
            return;
        }
        tracing::info!("offline: {why}");
        if let Some((_, task)) = self.events.take() {
            task.abort();
        }
        self.stop_browser();
        self.me = None;
        self.pending = None;
        self.link = control::Cloud::Offline;
        self.offline_since = None;
        self.address = None;
        self.peer_addresses = PeerAddresses::default();
        self.friends = FriendLists::default();
        self.friends_answers.ignore_pending();
        self.friends_dirty = true;
        self.sessions.clear();
        for answers in self.session_answers.values_mut() {
            answers.ignore_pending();
        }
        self.invitations.clear();
        self.share_reset();
        self.forget_social();
        if why != "offline" {
            self.discord_end(false);
        }
        self.links.clear();
        if let Some(tunnels) = self.tunnels.take() {
            tokio::spawn(async move { tunnels.close().await });
        }
        self.relays_expire = 0;
        self.restriction_answers.ignore_pending();
        self.restrictions_changed(Vec::new());
        if let Some(cloud) = &self.cloud
            && cloud.is_signed_in()
            && why != "offline"
        {
            cloud.forget();
        }
        self.state_changed();
        self.event(control::Event::Session { session: None });
    }

    fn cloud_event(&mut self, input: EventInput) {
        match input {
            EventInput::Connecting => {
                if self.link != control::Cloud::Online {
                    self.link = control::Cloud::Connecting;
                }
            }
            EventInput::Online => {
                self.link = control::Cloud::Online;
                self.offline_since = None;
                self.fetch_friends();
                self.push_presence();
                self.refresh_sessions();
                self.refresh_social();
                self.fetch_restrictions();
                self.share_fetch();
            }
            EventInput::Offline(reason) => {
                tracing::info!("event connection: {reason}");
                if self.link == control::Cloud::Online {
                    self.offline_since = Some(Instant::now());
                }
                self.link = control::Cloud::Offline;
            }
            EventInput::Event(event) => self.event_from_cloud(event),
            EventInput::SignedOut => {
                self.event(control::Event::Notice {
                    code: "signed_out".into(),
                    message: "This device was signed out.".into(),
                });
                self.end_online("signed out");
            }
            EventInput::ClientTooOld(minimum) => self.client_too_old(minimum),
            EventInput::Banned(restriction) => {
                let message = match restriction.until {
                    Some(until) => format!("This account is banned until {until} (Unix time)."),
                    None => "This account is banned.".to_owned(),
                };
                self.event(control::Event::Notice {
                    code: ErrorCode::Banned.as_str().into(),
                    message,
                });
                self.event(control::Event::Banned { restriction });
                self.end_online("banned");
            }
        }
        self.state_changed();
    }

    /// The cloud refuses this client for being too old. The launcher is told once per run (every
    /// command that fails the same way still carries the code); the event connection ends for
    /// good, so nothing keeps asking until the service is started again. The sign-in stays.
    fn client_too_old(&mut self, minimum: String) {
        if let Some((_, task)) = self.events.take() {
            task.abort();
        }
        self.link = control::Cloud::Offline;
        self.offline_since = None;
        if self.client_too_old.first() {
            tracing::warn!("the cloud requires a newer client: {minimum}");
            self.event(control::Event::ClientTooOld { minimum });
        }
        self.state_changed();
    }

    fn event_from_cloud(&mut self, event: Event) {
        match event {
            Event::Welcome { address, .. } => {
                self.address = address.parse().ok();
                self.bridge_state();
            }
            Event::Friends => self.fetch_friends(),
            Event::Presence { account, presence } => {
                if let Some(friend) = self
                    .friends
                    .friends
                    .iter_mut()
                    .find(|f| f.account.id == account)
                    && !is_older(presence.revision, friend.presence.revision)
                {
                    friend.presence = presence;
                    self.friends_dirty = true;
                }
            }
            Event::Invitation { invitation } => {
                self.invitations
                    .insert(invitation.id.clone(), invitation.clone());
                self.event(control::Event::Invitation { invitation });
            }
            Event::InvitationGone { id } => {
                if self.invitations.remove(&id).is_some() {
                    self.event(control::Event::InvitationGone { id });
                }
            }
            Event::Session { session } => {
                self.session_answers
                    .entry(session.id.clone())
                    .or_default()
                    .pushed();
                self.session_changed(session);
            }
            Event::SessionGone { id } => {
                self.session_answers.entry(id.clone()).or_default().pushed();
                self.session_closed(&id);
            }
            // A mute, a warning or the decision on an appeal: read what limits the account again.
            Event::Action { .. } => self.fetch_restrictions(),
            Event::SignedOut | Event::Banned { .. } | Event::Unknown => {}
            social => self.social_event(social),
        }
    }

    // Restrictions

    /// Reads the account's actions and tells the launcher which mutes still limit it.
    fn fetch_restrictions(&mut self) {
        let Some(cloud) = self.cloud.clone().filter(|_| self.me.is_some()) else {
            return;
        };
        let question = self.restriction_answers.ask();
        self.restrictions_asked = Some(Instant::now());
        self.spawn(
            async move { cloud.actions().await },
            move |service, result| {
                if !service.restriction_answers.take(question) {
                    return;
                }
                match result {
                    Ok(list) => {
                        let now = now();
                        let active = list
                            .actions
                            .iter()
                            .filter(|statement| statement.is_active(now))
                            .filter_map(control::Restriction::of_statement)
                            .collect();
                        service.restrictions_changed(active);
                    }
                    Err(CloudError::Offline(_)) => {}
                    Err(error) => {
                        service.failure(&error);
                    }
                }
            },
        );
    }

    /// Sends the launcher the limits when they differ from what it holds.
    fn restrictions_changed(&mut self, restrictions: Vec<control::Restriction>) {
        if self.restrictions != restrictions {
            self.restrictions = restrictions.clone();
            self.event(control::Event::Restrictions { restrictions });
        }
    }

    fn fetch_friends(&mut self) {
        let Some(cloud) = self.cloud.clone().filter(|_| self.me.is_some()) else {
            return;
        };
        // Several reads can be on their way, from signing in, going online and pushes; one
        // that arrives late must not bring back what changed since.
        let question = self.friends_answers.ask();
        self.spawn(
            async move { cloud.friends().await },
            move |service, result| match result {
                Ok(mut lists) if service.friends_answers.take(question) => {
                    // A presence pushed while the lists were on their way may be newer.
                    for friend in &mut lists.friends {
                        if let Some(known) = service
                            .friends
                            .friends
                            .iter()
                            .find(|f| f.account.id == friend.account.id)
                            && is_older(friend.presence.revision, known.presence.revision)
                        {
                            friend.presence = known.presence.clone();
                        }
                    }
                    service.friends = lists;
                    service.friends_dirty = true;
                }
                Ok(_) | Err(CloudError::Offline(_)) => {}
                Err(error) => {
                    service.failure(&error);
                }
            },
        );
    }

    /// Tells the cloud which kind of path a tunnel settled on, so it can count how often players
    /// reach each other directly. Fire and forget: whatever the answer is (an old cloud answers
    /// 404, an offline one nothing), nobody is told and nothing is retried. It carries no account,
    /// peer or time.
    fn report_path(&mut self, direct: bool, upgraded: bool) {
        let Some(cloud) = self.cloud.clone().filter(|_| self.me.is_some()) else {
            return;
        };
        self.spawn(
            async move { cloud.tunnel_path(direct, upgraded).await },
            |_service, _result| {},
        );
    }

    fn push_presence(&mut self) {
        let Some(cloud) = self.cloud.clone().filter(|_| self.me.is_some()) else {
            return;
        };
        let body = PresenceRequest {
            activity: self.activity.clone(),
        };
        self.spawn(
            async move { cloud.presence(&body).await },
            |service, result| {
                if let Err(error) = result
                    && !matches!(error, CloudError::Offline(_))
                {
                    service.failure(&error);
                }
            },
        );
    }

    /// Fetches each known session again, after the event connection was away.
    fn refresh_sessions(&mut self) {
        let Some(cloud) = self.cloud.clone() else {
            return;
        };
        for id in self.sessions.keys().cloned().collect::<Vec<_>>() {
            let cloud = cloud.clone();
            let question = self.session_answers.entry(id.clone()).or_default().ask();
            self.spawn(
                async move { (cloud.session(&id).await, id) },
                move |service, (result, id)| {
                    let answers = service.session_answers.entry(id.clone()).or_default();
                    if !answers.take(question) {
                        return;
                    }
                    match result {
                        Ok(session) => service.session_changed(session),
                        Err(CloudError::Api(error)) if error.error == ErrorCode::NotFound => {
                            service.session_closed(&id)
                        }
                        Err(_) => {}
                    }
                },
            );
        }
    }

    // Parties and messages

    fn social_command(&mut self, id: u64, command: Command) {
        let Some(cloud) = self.signed_in_cloud(id) else {
            return;
        };
        match command {
            Command::Party => self.request(id, async move { cloud.party().await }, |_, party| {
                control::PartyState {
                    party: party.map(launcher_party),
                }
            }),
            Command::PartyCreate => self.request(
                id,
                async move {
                    // In a party already: that one is what the player gets back.
                    match cloud.party().await? {
                        Some(party) => Ok(party),
                        None => cloud.create_party().await,
                    }
                },
                |service, party| {
                    service.party_changed(party.clone());
                    control::PartyState {
                        party: Some(launcher_party(party)),
                    }
                },
            ),
            Command::PartyInvite { accounts, call } => self.request(
                id,
                async move { cloud.invite_to_party(accounts, call).await },
                |_, invited| Invited { sent: invited.sent },
            ),
            Command::MissedCalls => self.request(
                id,
                async move { cloud.missed_calls().await },
                |_, missed| missed,
            ),
            Command::CallsSeen { account } => self.request(
                id,
                async move { cloud.calls_seen(account).await },
                |_, ()| Done {},
            ),
            Command::PartyLink { url } => self.request(
                id,
                async move {
                    let body = anyplay_api::UpdateParty {
                        leader: None,
                        discord: Some(url),
                    };
                    cloud.update_party(&body).await
                },
                |service, party| {
                    service.party_changed(party);
                    Done {}
                },
            ),
            Command::PartyAnswer {
                id: invitation,
                accept,
            } => {
                if !is_title_id(&invitation) {
                    self.fail(id, bad_id());
                    return;
                }
                if accept {
                    self.request(
                        id,
                        async move { cloud.accept_party_invitation(&invitation).await },
                        |service, party| {
                            service.party_invitations.retain(|_, i| i.party != party.id);
                            service.party_changed(party.clone());
                            control::PartyState {
                                party: Some(launcher_party(party)),
                            }
                        },
                    );
                } else {
                    self.drop_party_invitation(&invitation);
                    self.request(
                        id,
                        async move { cloud.decline_party_invitation(&invitation).await },
                        |_, ()| Done {},
                    );
                }
            }
            Command::PartyLeave => self.request(
                id,
                async move { cloud.leave_party().await },
                |service, ()| {
                    service.party_cleared();
                    Done {}
                },
            ),
            Command::PartyKick { account } => self.request(
                id,
                async move { cloud.kick_from_party(account).await },
                |_, ()| Done {},
            ),
            Command::PartyLeader { account } => self.request(
                id,
                async move {
                    let body = anyplay_api::UpdateParty {
                        leader: Some(account),
                        discord: None,
                    };
                    cloud.update_party(&body).await
                },
                |service, party| {
                    service.party_changed(party);
                    Done {}
                },
            ),
            Command::PartyMove { session } => {
                if !is_title_id(&session) {
                    self.fail(id, bad_id());
                    return;
                }
                self.request(
                    id,
                    async move { cloud.move_party(&session).await },
                    |_, invited| Invited { sent: invited.sent },
                );
            }
            Command::JoinCode => {
                self.request(id, async move { cloud.join_code().await }, |_, code| code)
            }
            Command::JoinCodeRevoke => self.request(
                id,
                async move { cloud.revoke_join_code().await },
                |_, ()| Done {},
            ),
            Command::JoinRedeem { code } => {
                let Some(code) = anyplay_api::join_code_from_text(&code) else {
                    self.fail(
                        id,
                        Failure::new(ErrorCode::BadRequest.as_str(), "That is not a join code."),
                    );
                    return;
                };
                self.request(
                    id,
                    async move { cloud.redeem_join_code(&code).await },
                    |service, redeemed| {
                        service.hold_redeemed(&redeemed);
                        redeemed
                    },
                );
            }
            Command::Conversations => self.request(
                id,
                async move { cloud.conversations().await },
                |_, conversations| conversations,
            ),
            Command::OpenConversation { accounts, name } => self.request(
                id,
                async move { cloud.open_conversation(accounts, name).await },
                |_, conversation| conversation,
            ),
            Command::Messages {
                conversation,
                after,
                before,
                limit,
                changed_after,
            } => {
                if !is_title_id(&conversation) {
                    self.fail(id, bad_id());
                    return;
                }
                if u8::from(after.is_some())
                    + u8::from(before.is_some())
                    + u8::from(changed_after.is_some())
                    > 1
                {
                    self.fail(
                        id,
                        Failure::new(
                            ErrorCode::BadRequest.as_str(),
                            "Give after, before or changed_after, one of them.",
                        ),
                    );
                    return;
                }
                self.request(
                    id,
                    async move {
                        cloud
                            .messages(&conversation, after, before, limit, changed_after)
                            .await
                    },
                    |_, messages| messages,
                );
            }
            Command::EditMessage {
                conversation,
                seq,
                text,
            } => {
                if !is_title_id(&conversation) {
                    self.fail(id, bad_id());
                    return;
                }
                if !anyplay_api::is_message_text(&text) {
                    self.fail(id, bad_message_text());
                    return;
                }
                self.request(
                    id,
                    async move { cloud.edit_message(&conversation, seq, &text).await },
                    |_, message| message,
                );
            }
            Command::UnsendMessage { conversation, seq } => {
                if !is_title_id(&conversation) {
                    self.fail(id, bad_id());
                    return;
                }
                self.request(
                    id,
                    async move { cloud.unsend_message(&conversation, seq).await },
                    |_, message| message,
                );
            }
            Command::ReactMessage {
                conversation,
                seq,
                reaction,
            } => {
                if !is_title_id(&conversation) {
                    self.fail(id, bad_id());
                    return;
                }
                self.request(
                    id,
                    async move { cloud.react_to_message(&conversation, seq, reaction).await },
                    |_, message| message,
                );
            }
            Command::Typing {
                conversation,
                typing,
            } => {
                if !is_title_id(&conversation) {
                    self.fail(id, bad_id());
                    return;
                }
                if typing {
                    if !self.typing.allow(&conversation, Instant::now()) {
                        self.ok(id, Done {});
                        return;
                    }
                } else {
                    self.typing.stop(&conversation);
                }
                self.request(
                    id,
                    async move { cloud.typing(&conversation, typing).await },
                    |_, ()| Done {},
                );
            }
            Command::ConversationSettings {
                conversation,
                muted,
                pinned,
            } => {
                if !is_title_id(&conversation) {
                    self.fail(id, bad_id());
                    return;
                }
                let settings = anyplay_api::ConversationSettings { muted, pinned };
                self.request(
                    id,
                    async move { cloud.conversation_settings(&conversation, &settings).await },
                    |_, conversation| conversation,
                );
            }
            Command::SendMessage {
                conversation,
                text,
                reply,
            } => {
                if !is_title_id(&conversation) {
                    self.fail(id, bad_id());
                    return;
                }
                if reply == Some(0) {
                    self.fail(id, bad_id());
                    return;
                }
                // Writing ends typing at the cloud; the next signal goes through at once.
                self.typing.stop(&conversation);
                if !anyplay_api::is_message_text(&text) {
                    self.fail(id, bad_message_text());
                    return;
                }
                self.request(
                    id,
                    async move { cloud.send_message(&conversation, &text, reply).await },
                    |_, message| message,
                );
            }
            Command::MarkRead { conversation, seq } => {
                if !is_title_id(&conversation) {
                    self.fail(id, bad_id());
                    return;
                }
                self.request(
                    id,
                    async move { cloud.mark_read(&conversation, seq).await },
                    |_, ()| Done {},
                );
            }
            Command::AcceptRequest { conversation } => {
                if !is_title_id(&conversation) {
                    self.fail(id, bad_id());
                    return;
                }
                self.request(
                    id,
                    async move { cloud.accept_conversation(&conversation).await },
                    |_, conversation| conversation,
                );
            }
            Command::LeaveConversation { conversation } => {
                if !is_title_id(&conversation) {
                    self.fail(id, bad_id());
                    return;
                }
                self.request(
                    id,
                    async move { cloud.leave_conversation(&conversation).await },
                    |_, ()| Done {},
                );
            }
            Command::AddToConversation {
                conversation,
                accounts,
            } => {
                if !is_title_id(&conversation) {
                    self.fail(id, bad_id());
                    return;
                }
                self.request(
                    id,
                    async move { cloud.add_to_conversation(&conversation, accounts).await },
                    |_, added| added,
                );
            }
            Command::SendMedia { conversation, path } => {
                if !is_title_id(&conversation) {
                    self.fail(id, bad_id());
                    return;
                }
                self.request(
                    id,
                    async move {
                        let (content_type, bytes) = read_media(path.into()).await?;
                        cloud.send_media(&conversation, content_type, bytes).await
                    },
                    |_, message| message,
                );
            }
            Command::FetchMedia { media, path } => {
                if !is_title_id(&media) {
                    self.fail(id, bad_id());
                    return;
                }
                self.request(
                    id,
                    async move {
                        cloud
                            .fetch_media(
                                &media,
                                std::path::PathBuf::from(path),
                                anyplay_api::MAX_CLIP_BYTES,
                            )
                            .await
                    },
                    |_, ()| Done {},
                );
            }
            Command::Lfg {
                title_id,
                mode,
                language,
            } => {
                if !is_title_id(&title_id) {
                    self.fail(id, bad_title());
                    return;
                }
                if language
                    .as_deref()
                    .is_some_and(|l| !anyplay_api::is_language_tag(l))
                {
                    self.fail(id, bad_language());
                    return;
                }
                self.request(
                    id,
                    async move { cloud.lfg(&title_id, mode, language.as_deref()).await },
                    |_, listings| listings,
                );
            }
            Command::LfgMine => {
                self.request(id, async move { cloud.lfg_mine().await }, |_, mine| mine);
            }
            Command::LfgPost {
                title_id,
                mode,
                language,
                note,
                slots,
            } => {
                if !is_title_id(&title_id) {
                    self.fail(id, bad_title());
                    return;
                }
                if language
                    .as_deref()
                    .is_some_and(|l| !anyplay_api::is_language_tag(l))
                {
                    self.fail(id, bad_language());
                    return;
                }
                if anyplay_api::check_lfg_note(&note).is_err() {
                    self.fail(
                        id,
                        Failure::new(
                            ErrorCode::BadRequest.as_str(),
                            "A note has at most 140 characters and no links.",
                        ),
                    );
                    return;
                }
                if !(1..=anyplay_api::MAX_LFG_SLOTS).contains(&slots) {
                    self.fail(
                        id,
                        Failure::new(
                            ErrorCode::BadRequest.as_str(),
                            "A listing looks for 1 to 7 players.",
                        ),
                    );
                    return;
                }
                let body = anyplay_api::CreateListing {
                    title_id,
                    mode,
                    language,
                    note,
                    slots,
                };
                self.request(
                    id,
                    async move { cloud.lfg_post(&body).await },
                    |_, listing| listing,
                );
            }
            Command::LfgClose { listing } => {
                if !is_title_id(&listing) {
                    self.fail(id, bad_id());
                    return;
                }
                self.request(
                    id,
                    async move { cloud.lfg_close(&listing).await },
                    |_, ()| Done {},
                );
            }
            Command::LfgAsk { listing } => {
                if !is_title_id(&listing) {
                    self.fail(id, bad_id());
                    return;
                }
                self.request(
                    id,
                    async move { cloud.lfg_ask(&listing).await },
                    |_, listing| listing,
                );
            }
            Command::LfgWithdraw { listing } => {
                if !is_title_id(&listing) {
                    self.fail(id, bad_id());
                    return;
                }
                self.request(
                    id,
                    async move { cloud.lfg_withdraw(&listing).await },
                    |_, ()| Done {},
                );
            }
            Command::LfgAnswer {
                listing,
                account,
                accept,
            } => {
                if !is_title_id(&listing) {
                    self.fail(id, bad_id());
                    return;
                }
                self.request(
                    id,
                    async move { cloud.lfg_answer(&listing, account, accept).await },
                    |_, listing| listing,
                );
            }
            _ => self.fail(
                id,
                Failure::new(Failure::UNKNOWN_COMMAND, "Not a social command."),
            ),
        }
    }

    /// News of parties and messages from the cloud. Messages arrive as pointers; the launcher
    /// fetches them.
    fn social_event(&mut self, event: Event) {
        match event {
            Event::Party { party } => {
                self.party_answers.pushed();
                self.party_changed(party);
            }
            Event::PartyGone { id } => {
                self.party_answers.pushed();
                if self.party.as_ref().is_some_and(|p| p.id == id) {
                    self.party_cleared();
                }
            }
            Event::PartyInvitation { invitation } => {
                self.party_invitations
                    .insert(invitation.id.clone(), invitation.clone());
                self.event(control::Event::PartyInvitation { invitation });
            }
            Event::PartyInvitationGone { id } => self.drop_party_invitation(&id),
            Event::MissedCall { call } => self.event(control::Event::MissedCall { call }),
            Event::MissedCallGone { account } => {
                self.event(control::Event::MissedCallGone { account });
            }
            Event::CallEnded { account, reason } => {
                self.event(control::Event::CallEnded { account, reason });
            }
            Event::Message { conversation, seq } => {
                self.event(control::Event::Message { conversation, seq });
            }
            Event::MessageRequest { conversation } => {
                self.event(control::Event::MessageRequest { conversation });
            }
            Event::Read {
                conversation,
                account,
                seq,
            } => self.event(control::Event::Read {
                conversation,
                account,
                seq,
            }),
            Event::Delivered {
                conversation,
                account,
                seq,
            } => self.event(control::Event::Delivered {
                conversation,
                account,
                seq,
            }),
            Event::Typing {
                conversation,
                account,
                typing,
            } => self.event(control::Event::Typing {
                conversation,
                account,
                typing,
            }),
            Event::MessageUpdate {
                conversation,
                seq,
                rev,
            } => self.event(control::Event::MessageUpdate {
                conversation,
                seq,
                rev,
            }),
            Event::Conversation { id } => self.event(control::Event::Conversation { id }),
            Event::ConversationGone { id } => {
                self.event(control::Event::ConversationGone { id });
            }
            Event::LfgRequest {
                listing,
                title_id,
                from,
            } => self.event(control::Event::LfgRequest {
                listing,
                title_id,
                from,
            }),
            Event::LfgAccepted {
                listing,
                title_id,
                by,
            } => self.event(control::Event::LfgAccepted {
                listing,
                title_id,
                by,
            }),
            event @ (Event::ShareInvitation { .. }
            | Event::ShareInvitationGone { .. }
            | Event::Share { .. }
            | Event::ShareGone { .. }) => self.share_cloud_event(event),
            Event::Feed => self.feed_dirty = true,
            Event::AppearOffline { appear_offline } => self.set_appear_offline(appear_offline),
            _ => {}
        }
    }

    /// Takes a party the cloud sent unless a newer copy of it is held already.
    fn party_changed(&mut self, party: anyplay_api::Party) {
        if let Some(known) = &self.party
            && known.id == party.id
            && is_older(party.revision, known.revision)
        {
            return;
        }
        self.party = Some(party.clone());
        self.event(control::Event::Party {
            party: Some(launcher_party(party)),
        });
        self.voice_sync();
        self.share_party_changed();
    }

    fn party_cleared(&mut self) {
        if self.party.take().is_some() {
            self.event(control::Event::Party { party: None });
        }
        self.voice.announced = None;
        self.voice_sync();
        self.share_party_changed();
    }

    /// Holds the invitation a redeemed join code made, as if it had arrived as an event, so that
    /// `answer_invitation` and `party_answer` find it. The cloud also sends the event, which the
    /// launcher sees; nothing is announced here.
    fn hold_redeemed(&mut self, redeemed: &anyplay_api::Redeemed) {
        match redeemed {
            anyplay_api::Redeemed::Session(invitation) => {
                self.invitations
                    .insert(invitation.id.clone(), invitation.clone());
            }
            anyplay_api::Redeemed::Party(invitation) => {
                self.party_invitations
                    .insert(invitation.id.clone(), invitation.clone());
            }
        }
    }

    fn drop_party_invitation(&mut self, id: &str) {
        if self.party_invitations.remove(id).is_some() {
            self.event(control::Event::PartyInvitationGone { id: id.to_owned() });
        }
    }

    /// Reads party, party invitations and conversations again, after signing in and after the
    /// event connection was away. The launcher compares the conversations' newest numbers with
    /// the ones it holds and fetches what it missed.
    fn refresh_social(&mut self) {
        let Some(cloud) = self.cloud.clone().filter(|_| self.me.is_some()) else {
            return;
        };
        let question = self.party_answers.ask();
        self.voice.announced = None;
        let party = cloud.clone();
        self.spawn(
            async move {
                let party_now = party.party().await;
                let invitations = party.party_invitations().await;
                (party_now, invitations)
            },
            move |service, (party, invitations)| {
                if !service.party_answers.take(question) {
                    return;
                }
                match party {
                    Ok(Some(party)) => service.party_changed(party),
                    Ok(None) => service.party_cleared(),
                    Err(CloudError::Offline(_)) => {}
                    Err(error) => {
                        service.failure(&error);
                    }
                }
                if let Ok(list) = invitations {
                    let now = now();
                    let known: Vec<String> = service.party_invitations.keys().cloned().collect();
                    for id in known {
                        if !list.invitations.iter().any(|i| i.id == id) {
                            service.drop_party_invitation(&id);
                        }
                    }
                    for invitation in list.invitations.into_iter().filter(|i| i.expires > now) {
                        if !service.party_invitations.contains_key(&invitation.id) {
                            service
                                .party_invitations
                                .insert(invitation.id.clone(), invitation.clone());
                            service.event(control::Event::PartyInvitation { invitation });
                        }
                    }
                }
            },
        );
        self.spawn(
            {
                let cloud = cloud.clone();
                async move { cloud.missed_calls().await }
            },
            |service, result| match result {
                Ok(missed) => service.event(control::Event::MissedCalls(missed)),
                Err(CloudError::Offline(_)) => {}
                Err(error) => {
                    service.failure(&error);
                }
            },
        );
        let question = self.conversation_answers.ask();
        self.spawn(
            async move { cloud.conversations().await },
            move |service, result| match result {
                Ok(conversations) if service.conversation_answers.take(question) => {
                    service.event(control::Event::Conversations(conversations));
                }
                Ok(_) | Err(CloudError::Offline(_)) => {}
                Err(error) => {
                    service.failure(&error);
                }
            },
        );
    }

    /// Forgets the party and its invitations, as after a sign-out.
    fn forget_social(&mut self) {
        self.party_answers.ignore_pending();
        self.conversation_answers.ignore_pending();
        self.party_invitations.clear();
        self.party_cleared();
    }

    // Share screen

    /// Whether the screen hub should run: signed in with tunnels up and in a party.
    fn screen_wanted(&self) -> bool {
        self.me.is_some() && self.tunnels.is_some() && self.party.is_some()
    }

    /// The other members of the party that a stream may reach, by tunnel key.
    fn screen_members(&self) -> screen::Members {
        self.voice_members()
            .into_iter()
            .map(|(endpoint, member)| {
                (
                    endpoint,
                    screen::Member {
                        account: member.account,
                        relay: member.relay,
                    },
                )
            })
            .collect()
    }

    /// Starts the hub when the player is in a party, follows the members, ends it otherwise.
    fn screen_sync(&mut self) {
        self.share_sync();
        if !self.screen_wanted() {
            if self.screen.hub.take().is_some() {
                self.screen.generation += 1;
                self.screen.view = screen::View::default();
                self.screen_event();
            }
            return;
        }
        if let Some(hub) = &self.screen.hub {
            hub.set_members(self.screen_members());
            return;
        }
        let Some(tunnels) = self.tunnels.clone() else {
            return;
        };
        self.screen.generation += 1;
        let generation = self.screen.generation;
        let (reports, mut rx) = mpsc::unbounded_channel();
        let inbox = self.inbox.clone();
        tokio::spawn(async move {
            while let Some(output) = rx.recv().await {
                if inbox.send(Input::Screen(generation, output)).is_err() {
                    return;
                }
            }
        });
        self.screen.hub = Some(screen::Hub::start(
            &tunnels,
            self.config.workers.shared(),
            self.screen_members(),
            reports,
        ));
        self.screen_event();
    }

    fn screen_output(&mut self, output: screen::Output) {
        match output {
            screen::Output::View(view) => {
                self.screen.view = view;
                self.screen_event();
            }
            screen::Output::Notice { code, message } => {
                self.event(control::Event::Notice {
                    code: code.into(),
                    message,
                });
            }
        }
    }

    /// The `screen` event: the hub's view and what the player could do now.
    fn screen_event(&self) {
        let capabilities = self.config.workers.capabilities();
        let others = self
            .party
            .as_ref()
            .is_some_and(|party| party.members.len() > 1);
        let can_share = self.screen.hub.is_some()
            && self.game.is_some()
            && others
            && capabilities
                .as_ref()
                .is_some_and(|c| !c.encode.is_empty() && c.capture);
        let can_watch = self.screen.hub.is_some()
            && capabilities.as_ref().is_some_and(|c| !c.decode.is_empty());
        self.event(control::Event::Screen {
            can_share,
            can_watch,
            hosting: self.screen.view.hosting.clone(),
            offers: self.screen.view.offers.clone(),
            watching: self.screen.view.watching.clone(),
        });
    }

    fn screen_start(
        &mut self,
        id: u64,
        source: control::ScreenSource,
        window: Option<i64>,
        pids: Vec<u32>,
    ) {
        if self.me.is_none() {
            self.fail(id, Failure::new(Failure::NOT_SIGNED_IN, "Sign in first."));
            return;
        }
        let Some(title) = self.game.as_ref().map(|g| g.title.clone()) else {
            self.fail(id, Failure::new(Failure::NO_GAME, "No game is running."));
            return;
        };
        let Some(hub) = self.screen.hub.as_ref().map(|h| h.handle()) else {
            self.fail(
                id,
                Failure::new(Failure::NOT_IN_PARTY, "Join a party to share your screen."),
            );
            return;
        };
        self.spawn(
            async move {
                hub.start_sharing_game(title, source, screen::GameWindow { window, pids })
                    .await
            },
            move |service, result| service.screen_answer(id, result),
        );
    }

    fn screen_watch(&mut self, id: u64, account: AccountId) {
        let Some(hub) = self.screen.hub.as_ref().map(|h| h.handle()) else {
            self.fail(
                id,
                Failure::new(Failure::NOT_IN_PARTY, "Join a party to watch a screen."),
            );
            return;
        };
        self.spawn(
            async move { hub.watch(account.0).await },
            move |service, result| service.screen_answer(id, result),
        );
    }

    fn screen_answer(&mut self, id: u64, result: Result<(), screen::Refusal>) {
        match result {
            Ok(()) => self.ok(id, Done {}),
            Err(refusal) => {
                let failure = match refusal {
                    screen::Refusal::NoEncoder => {
                        Failure::new(Failure::NO_ENCODER, "This PC cannot share its screen yet.")
                    }
                    screen::Refusal::NoDecoder => Failure::new(
                        Failure::NO_DECODER,
                        "This PC cannot show a shared screen yet.",
                    ),
                    screen::Refusal::Busy => {
                        Failure::new(Failure::BUSY, "A shared screen is running already.")
                    }
                    screen::Refusal::NotFound => Failure::new(
                        ErrorCode::NotFound.as_str(),
                        "That member does not share a screen.",
                    ),
                };
                self.fail(id, failure);
            }
        }
    }

    // Voice

    /// Whether voice should run now: the player switched it on, is signed in with tunnels up,
    /// and is in a party.
    fn voice_wanted(&self) -> bool {
        self.voice.enabled && self.me.is_some() && self.tunnels.is_some() && self.party.is_some()
    }

    /// The other members of the party that voice may connect to, by tunnel key.
    fn voice_members(&self) -> voice::Members {
        match (&self.party, &self.me) {
            (Some(party), Some(me)) => members_of(party, me.id, self.secret.public()),
            _ => voice::Members::new(),
        }
    }

    /// Brings voice in line with what is wanted: opens the devices and joins when the player is
    /// in a party with voice on, follows the party's members, leaves otherwise.
    fn voice_sync(&mut self) {
        self.screen_sync();
        if !self.voice_wanted() {
            let was_on = self.voice.hub.take().is_some() || self.voice.opening;
            self.voice.opening = false;
            self.voice.generation += 1;
            self.voice.report = None;
            if was_on {
                self.voice_event();
            }
            self.party_announce();
            return;
        }
        if let Some(hub) = &self.voice.hub {
            hub.set_members(self.voice_members());
        } else if !self.voice.opening {
            self.voice_open();
        }
        self.party_announce();
    }

    /// Hangs up and joins again, with other devices.
    fn voice_restart(&mut self) {
        self.voice.hub = None;
        self.voice.opening = false;
        self.voice.generation += 1;
        self.voice.report = None;
        self.voice_sync();
        self.voice_event();
    }

    /// Opens the sound devices off the loop; `voice_opened` takes the result.
    fn voice_open(&mut self) {
        self.voice.opening = true;
        self.voice.generation += 1;
        let generation = self.voice.generation;
        let audio = self.config.audio.clone();
        let (input, output) = (self.voice.input.clone(), self.voice.output.clone());
        let mode = self.voice.settings.mode;
        let noise = self.voice.settings.noise_suppression;
        let echo = self.voice.settings.echo_cancellation;
        self.spawn(
            async move {
                tokio::task::spawn_blocking(move || {
                    let device = audio.open(input.as_deref(), output.as_deref())?;
                    let config = anyplay_voice::Config {
                        mode,
                        noise_suppression: noise.unwrap_or(true),
                        echo_cancellation: echo.unwrap_or(true),
                        ..anyplay_voice::Config::default()
                    };
                    Engine::new(config, device).map_err(|e| e.to_string())
                })
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
            },
            move |service, result| service.voice_opened(generation, result),
        );
        self.voice_event();
    }

    fn voice_opened(&mut self, generation: u64, result: Result<Engine, String>) {
        if generation != self.voice.generation {
            return; // the engine drops, and the devices with it
        }
        self.voice.opening = false;
        let engine = match result {
            Ok(engine) if self.voice_wanted() => engine,
            Ok(_) => return self.voice_event(),
            Err(error) => {
                tracing::warn!("voice: {error}");
                self.voice.enabled = false;
                self.event(control::Event::Notice {
                    code: "voice_device".into(),
                    message: format!("Voice could not start: {error}"),
                });
                self.voice_event();
                self.party_announce();
                return;
            }
        };
        let Some(tunnels) = self.tunnels.clone() else {
            return;
        };
        let (reports, mut rx) = mpsc::unbounded_channel();
        let inbox = self.inbox.clone();
        tokio::spawn(async move {
            while let Some(report) = rx.recv().await {
                if inbox.send(Input::Voice(generation, report)).is_err() {
                    return;
                }
            }
        });
        self.voice.hub = Some(Hub::start(
            &tunnels,
            engine,
            self.voice.settings.clone(),
            self.voice_members(),
            reports,
        ));
        self.party_announce();
        self.voice_event();
    }

    /// The `voice` event: the state, and the levels of the last report while voice runs.
    fn voice_event(&self) {
        let report = self.voice.report.as_ref();
        let state = if self.voice.hub.is_some() {
            report.map_or(control::VoiceState::Connecting, |r| r.state)
        } else if self.voice.opening {
            control::VoiceState::Connecting
        } else {
            control::VoiceState::Off
        };
        let level = |level: anyplay_voice::Level| control::VoiceLevel {
            level: (level.rms * 100.0).round().clamp(0.0, 100.0) as u8,
            speaking: level.speaking,
        };
        self.event(control::Event::Voice {
            state,
            enabled: self.voice.enabled,
            mode: match self.voice.settings.mode {
                anyplay_voice::TalkMode::PushToTalk => control::VoiceMode::PushToTalk,
                anyplay_voice::TalkMode::VoiceActivity => control::VoiceMode::VoiceActivity,
            },
            muted: self.voice.settings.mic_muted,
            deafened: self.voice.settings.deafened,
            me: report.map(|r| level(r.me)).unwrap_or_default(),
            peers: report
                .map(|r| {
                    r.peers
                        .iter()
                        .map(|p| control::VoicePeer {
                            account: AccountId(p.account),
                            level: level(p.level),
                            link: p.link,
                        })
                        .collect()
                })
                .unwrap_or_default(),
        });
    }

    /// Tells the party where this net service is and whether it takes part in voice, when that
    /// differs from what the party holds and from what was told last.
    fn party_announce(&mut self) {
        let (Some(cloud), Some(party), Some(me), Some(_), Some((endpoint, relay))) = (
            self.cloud.clone(),
            self.party.as_ref(),
            self.me.as_ref(),
            self.tunnels.as_ref(),
            self.endpoint(),
        ) else {
            return;
        };
        let body = anyplay_api::PartyEndpoint {
            endpoint,
            relay,
            voice: self.voice.hub.is_some(),
        };
        let current = (party.id.clone(), body.clone());
        if self.voice.announced.as_ref() == Some(&current) {
            return;
        }
        let held = party.members.iter().find(|m| m.account.id == me.id);
        let known = held.is_some_and(|m| {
            m.endpoint == body.endpoint && m.relay == body.relay && m.voice == body.voice
        });
        let in_party = held.is_some();
        self.voice.announced = Some(current);
        if known || !in_party {
            return;
        }
        self.spawn(
            async move { cloud.party_endpoint(&body).await },
            |service, result| match result {
                Ok(party) => service.party_changed(party),
                Err(error) => {
                    tracing::info!("party endpoint: {error}");
                    service.voice.announced = None;
                }
            },
        );
    }

    // Relays and tunnels

    fn ask_relays(&mut self) {
        let Some(cloud) = self.cloud.clone().filter(|_| self.me.is_some()) else {
            return;
        };
        if self
            .relays_asked
            .is_some_and(|at| at.elapsed() < Duration::from_secs(30))
        {
            return;
        }
        self.relays_asked = Some(Instant::now());
        let endpoint = self.secret.public().to_string();
        self.spawn(
            async move { cloud.relay_tickets(&endpoint).await },
            |service, result| {
                let relays = match result {
                    Ok(relays) => relays,
                    Err(error) => {
                        tracing::info!("relay tickets: {error}");
                        if !matches!(error, CloudError::Offline(_)) {
                            service.failure(&error);
                        }
                        return;
                    }
                };
                service.relays_expire = relays.relays.iter().map(|r| r.expires).min().unwrap_or(0);
                let access: Vec<RelayAccess> = relays
                    .relays
                    .into_iter()
                    .filter_map(|relay| {
                        Some(RelayAccess {
                            url: relay.url.parse().ok()?,
                            token: relay.token,
                        })
                    })
                    .collect();
                service.use_relays(access);
            },
        );
    }

    /// Closes the tunnels and starts them again with fresh tickets and the current settings.
    /// The peers reconnect on their own.
    fn restart_tunnels(&mut self) {
        if let Some(tunnels) = self.tunnels.take() {
            tokio::spawn(async move { tunnels.close().await });
        }
        // Voice rides on the endpoint that just closed; it starts again with the new one.
        self.voice_sync();
        self.relays_asked = None;
        self.ask_relays();
    }

    fn use_relays(&mut self, relays: Vec<RelayAccess>) {
        if let Some(tunnels) = self.tunnels.clone() {
            tokio::spawn(async move { tunnels.set_relays(&relays).await });
            return;
        }
        if self.tunnels_starting || self.me.is_none() {
            return;
        }
        self.tunnels_starting = true;
        let (changes, mut rx) = mpsc::unbounded_channel();
        let inbox = self.inbox.clone();
        tokio::spawn(async move {
            while let Some(change) = rx.recv().await {
                if inbox.send(Input::Link(change)).is_err() {
                    return;
                }
            }
        });
        let relay_only = self.config.relay_only;
        let options = tunnel::Options {
            secret: self.secret.clone(),
            relays,
            relay_roots: self.config.relay_roots.clone(),
            bind: self.config.bind,
            relay_only,
            relay_quic_port: None,
        };
        let traffic = self.traffic.clone();
        self.spawn(
            async move {
                let tunnels = Tunnels::start(options, traffic, changes).await?;
                tunnels.online(Duration::from_secs(10)).await;
                Ok::<_, String>(tunnels)
            },
            move |service, result| {
                service.tunnels_starting = false;
                match result {
                    Ok(tunnels)
                        if service.me.is_some() && service.config.relay_only == relay_only =>
                    {
                        tracing::info!("tunnel endpoint {}", tunnels.id());
                        service.tunnels_generation += 1;
                        service.reach = None;
                        let generation = service.tunnels_generation;
                        let inbox = service.inbox.clone();
                        tunnels.watch_reach(move |reach| {
                            let _ = inbox.send(Input::Reach(generation, reach));
                        });
                        service.tunnels = Some(tunnels);
                        service.update_peers();
                        service.update_endpoints();
                        service.voice_sync();
                        service.bridge_state();
                    }
                    Ok(tunnels) => {
                        tokio::spawn(async move { tunnels.close().await });
                        // Relay only changed while they started: start them again with it.
                        if service.me.is_some() {
                            service.restart_tunnels();
                        }
                    }
                    Err(error) => tracing::warn!("tunnels could not start: {error}"),
                }
            },
        );
    }

    /// The tunnel key and relay of this net service, as sessions name them.
    /// The key is known from the start; the relay only once the tunnels run, and sessions learn
    /// it then through `update_endpoints`.
    fn endpoint(&self) -> Option<(String, Option<String>)> {
        let relay = self
            .tunnels
            .as_ref()
            .and_then(Tunnels::home_relay)
            .map(|url| url.to_string());
        Some((self.secret.public().to_string(), relay))
    }

    /// Tells every session whose record of this endpoint is out of date where it is now.
    fn update_endpoints(&mut self) {
        let (Some(cloud), Some((endpoint, relay)), Some(me)) =
            (self.cloud.clone(), self.endpoint(), self.me.as_ref())
        else {
            return;
        };
        let me = me.id;
        self.party_announce();
        for session in self.sessions.values() {
            let stale = session
                .members
                .iter()
                .find(|m| m.account.id == me)
                .is_some_and(|m| m.endpoint != endpoint || m.relay != relay);
            if !endpoint_push_due(
                &mut self.pushed_endpoints,
                &session.id,
                stale,
                &endpoint,
                &relay,
            ) {
                continue;
            }
            let cloud = cloud.clone();
            let id = session.id.clone();
            let failed = id.clone();
            let body = JoinSession {
                version: session.version.clone(),
                build: session.build.clone(),
                endpoint: endpoint.clone(),
                relay: relay.clone(),
                invitation: None,
                // The session's own version: it fits as it is.
                version_rule: VersionRule::Exact,
                override_version: false,
            };
            self.spawn(
                async move { cloud.join(&id, &body).await },
                move |service, result| match result {
                    Ok(session) => service.session_changed(session),
                    // A failed announcement may be tried again.
                    Err(_) => {
                        service.pushed_endpoints.remove(&failed);
                    }
                },
            );
        }
    }

    fn update_peers(&mut self) {
        let me = self.me.as_ref().map(|m| m.id);
        self.peer_addresses.refresh(me.map(|m| m.0), &self.sessions);
        let Some(tunnels) = &self.tunnels else {
            return;
        };
        let own = self.secret.public();
        let mut named = Vec::new();
        for session in self.sessions.values() {
            for member in &session.members {
                if Some(member.account.id) == me {
                    continue;
                }
                let (Ok(endpoint), Some(address)) = (
                    member.endpoint.parse::<EndpointId>(),
                    self.peer_addresses.of(member.account.id.0),
                ) else {
                    continue;
                };
                named.push((
                    endpoint,
                    member.account.id.0,
                    tunnel::Peer {
                        account: member.account.id.0,
                        handle: member.account.handle.clone(),
                        address,
                        relay: member
                            .relay
                            .as_deref()
                            .and_then(|r| r.parse::<RelayUrl>().ok()),
                    },
                ));
            }
        }
        let peers = unambiguous_endpoints(named, own);
        tunnels.set_peers(peers);
        self.sync_audience();
    }

    /// Tells the traffic path who a broadcast of the game reaches: the other members of the
    /// session the launcher shows, the one that gave the player its address. Never another
    /// session's members, and no one when the player is in none.
    fn sync_audience(&self) {
        let me = self.me.as_ref().map(|m| m.id);
        let audience: std::collections::HashSet<std::net::Ipv4Addr> = self
            .current_session()
            .and_then(|id| self.sessions.get(&id))
            .map(|session| {
                session
                    .members
                    .iter()
                    .filter(|m| Some(m.account.id) != me)
                    .filter_map(|m| self.peer_addresses.of(m.account.id.0))
                    .collect()
            })
            .unwrap_or_default();
        self.traffic.set_inbound(audience.clone());
        self.traffic.set_audience(audience);
    }

    fn link_change(&mut self, change: LinkChange) {
        // One line each time a tunnel settles on a path, with the totals so far. It names no one,
        // so an operator can collect these from test machines and work out the direct share.
        let before = self.links.get(&change.account).copied();
        if self.link_totals.record(before, change.link) {
            self.report_path(change.link == Link::Direct, before == Some(Link::Relay));
            let totals = self.link_totals;
            tracing::info!(
                target: "anyplay::path",
                path = if change.link == Link::Direct { "direct" } else { "relay" },
                direct = totals.direct,
                relay = totals.relay,
                upgrades = totals.upgrades,
                "tunnel path"
            );
        }
        if change.link == Link::Closed {
            self.links.remove(&change.account);
        } else {
            self.links.insert(change.account, change.link);
        }
        let state = match change.link {
            Link::Connecting => PeerState::Connecting,
            Link::Direct | Link::Relay => PeerState::Connected,
            Link::Closed => PeerState::Disconnected,
        };
        let sessions: Vec<Session> = self
            .bridge
            .as_ref()
            .map(|bridge| {
                bridge
                    .activated
                    .iter()
                    .filter_map(|id| self.sessions.get(id).cloned())
                    .collect()
            })
            .unwrap_or_default();
        for session in sessions {
            if let Some(member) = session
                .members
                .iter()
                .find(|m| m.account.id.0 == change.account)
            {
                self.bridge_event(Message::Peer(bridge::Peer {
                    session: session.id.clone(),
                    account_id: member.account.id.0,
                    handle: member.account.handle.clone(),
                    address: self.peer_address(member),
                    peer_state: state,
                }));
            }
        }
        self.session_view();
    }

    // Sessions

    fn session_changed(&mut self, session: Session) {
        let id = session.id.clone();
        // Another node may tell of an older change after a newer one.
        if self
            .sessions
            .get(&id)
            .is_some_and(|known| is_older(session.revision, known.revision))
        {
            return;
        }
        let before = self.sessions.insert(id.clone(), session);
        if self.is_activated(&id) {
            let after = &self.sessions[&id];
            let left: Vec<_> = before
                .iter()
                .flat_map(|b| b.members.iter())
                .filter(|m| !after.members.iter().any(|a| a.account.id == m.account.id))
                .cloned()
                .collect();
            let joined = after.members.iter().any(|a| {
                before
                    .as_ref()
                    .is_none_or(|b| !b.members.iter().any(|m| m.account.id == a.account.id))
            });
            self.peers_left(&id, &left);
            self.update_peers();
            if joined {
                self.announce_peers(&id);
            }
        } else {
            self.update_peers();
        }
        self.bridge_state();
        self.update_endpoints();
        self.session_view();
    }

    fn session_gone(&mut self, id: &str) {
        self.pushed_endpoints.remove(id);
        let before = self.sessions.remove(id);
        if self.is_activated(id)
            && let Some(before) = before
        {
            self.peers_left(id, &before.members);
        }
        if let Some(game) = &mut self.game {
            game.sessions.remove(id);
        }
        if let Some(bridge) = &mut self.bridge {
            bridge.activated.remove(id);
        }
        self.update_peers();
        self.bridge_state();
        self.session_view();
    }

    /// What the game calls this peer: the address this PC holds for the account, else the one
    /// the cloud named.
    fn peer_address(&self, member: &anyplay_api::Member) -> Ipv4Addr {
        self.peer_addresses
            .of(member.account.id.0)
            .or_else(|| member.address.parse().ok())
            .unwrap_or(Ipv4Addr::UNSPECIFIED)
    }

    /// The player's own address: the one the session the launcher shows gives it, else the one
    /// `welcome` named, which is what the cloud gives it in any session where it is free.
    fn own_address(&self) -> Ipv4Addr {
        let me = self.me.as_ref().map(|m| m.id);
        self.current_session()
            .and_then(|id| self.sessions.get(&id))
            .and_then(|session| session.members.iter().find(|m| Some(m.account.id) == me))
            .and_then(|member| member.address.parse().ok())
            .filter(|address| anyplay_api::is_assignable(*address))
            .or(self.address)
            .unwrap_or(Ipv4Addr::UNSPECIFIED)
    }

    fn is_activated(&self, session: &str) -> bool {
        self.bridge
            .as_ref()
            .is_some_and(|bridge| bridge.activated.contains(session))
    }

    /// Tells the game that these members are gone from an activated session.
    fn peers_left(&self, session: &str, members: &[anyplay_api::Member]) {
        let me = self.me.as_ref().map(|m| m.id);
        for member in members.iter().filter(|m| Some(m.account.id) != me) {
            self.bridge_event(Message::Peer(bridge::Peer {
                session: session.to_owned(),
                account_id: member.account.id.0,
                handle: member.account.handle.clone(),
                address: self.peer_address(member),
                peer_state: PeerState::Disconnected,
            }));
        }
    }

    /// The session the launcher shows: the running game's newest, else the newest of all.
    fn current_session(&self) -> Option<String> {
        let game: Option<&Session> = self.game.as_ref().and_then(|game| {
            game.sessions
                .iter()
                .filter_map(|id| self.sessions.get(id))
                .max_by_key(|s| s.created)
        });
        game.or_else(|| self.sessions.values().max_by_key(|s| s.created))
            .map(|s| s.id.clone())
    }

    fn session_view(&self) {
        let me = self.me.as_ref().map(|m| m.id);
        let session = self
            .current_session()
            .and_then(|id| self.sessions.get(&id))
            .map(|session| control::Session {
                id: session.id.clone(),
                kind: session.kind,
                title_id: session.title_id.clone(),
                title: session.title.clone(),
                version: session.version.clone(),
                leader: session.leader,
                max_members: session.max_members,
                visibility: session.visibility,
                created: session.created,
                members: session
                    .members
                    .iter()
                    .map(|member| control::Member {
                        account: member.account.clone(),
                        address: if Some(member.account.id) == me {
                            member.address.clone()
                        } else {
                            self.peer_address(member).to_string()
                        },
                        joined: member.joined,
                        link: (Some(member.account.id) != me).then(|| {
                            match self.links.get(&member.account.id.0) {
                                Some(Link::Direct) => control::Link::Direct,
                                Some(Link::Relay) => control::Link::Relay,
                                _ => control::Link::Connecting,
                            }
                        }),
                    })
                    .collect(),
            });
        self.event(control::Event::Session { session });
    }

    // Games

    #[allow(clippy::too_many_arguments)]
    fn game_start(
        &mut self,
        id: u64,
        title_id: String,
        title: String,
        version: String,
        build: String,
        join: Option<String>,
        host: Option<control::Host>,
        (version_rule, override_version): (VersionRule, bool),
    ) {
        if self.game.is_some() {
            self.game_end();
        }
        let hosted = (title_id.clone(), title.clone());
        let token: [u8; 32] = rand::random();
        let mut env = std::collections::BTreeMap::new();
        env.insert(
            "ANYPS5_NET_ENDPOINT".to_owned(),
            format!("127.0.0.1:{}", self.bridge_port),
        );
        env.insert(
            "ANYPS5_NET_TOKEN".to_owned(),
            data_encoding::HEXLOWER.encode(&token),
        );
        self.activity = Some(Activity {
            title_id: title_id.clone(),
            title: title.clone(),
            version: version.clone(),
            session: None,
        });
        self.bridge_tokens.set(Some(token.to_vec()));
        self.game = Some(GameRun {
            title_id,
            title,
            version: version.clone(),
            build: build.clone(),
            token: token.to_vec(),
            join: None,
            intent_sent: false,
            sessions: HashSet::new(),
        });
        self.push_presence();
        self.screen_event();
        self.share_changed();
        let Some(session) = join else {
            match host {
                Some(host) => self.host_session(id, env, host, hosted, version, build),
                None => self.ok(id, GameStarted { env, session: None }),
            }
            return;
        };
        let (Some(cloud), Some((endpoint, relay))) = (
            self.cloud.clone().filter(|_| self.me.is_some()),
            self.endpoint(),
        ) else {
            self.fail(
                id,
                Failure::new(Failure::OFFLINE, "The network is not ready to join yet."),
            );
            return;
        };
        let body = JoinSession {
            version,
            build,
            endpoint,
            relay,
            invitation: self
                .invitations
                .values()
                .find(|i| i.session == session)
                .map(|i| i.id.clone()),
            version_rule,
            override_version,
        };
        if override_version {
            tracing::info!("joining {session} although the version rule may not fit (Try Anyway)");
        }
        let game_token = token.to_vec();
        self.spawn(
            async move { cloud.join(&session, &body).await },
            move |service, result| match result {
                // The game this join was for ended or was replaced while the cloud answered:
                // the session belongs to no game, so it is left at once.
                Ok(joined) if !game_is(service.game.as_ref(), &game_token) => {
                    service.leave_orphan(joined.id);
                    service.ok(id, GameStarted { env, session: None });
                }
                Ok(joined) => {
                    let joined_id = joined.id.clone();
                    service
                        .invitations
                        .retain(|_, invitation| invitation.session != joined_id);
                    if let Some(game) = &mut service.game {
                        game.join = Some(joined_id.clone());
                        game.sessions.insert(joined_id);
                    }
                    service.session_changed(joined);
                    service.send_intent();
                    service.ok(id, GameStarted { env, session: None });
                }
                Err(error) => {
                    let failure = service.failure(&error);
                    service.fail(id, failure);
                }
            },
        );
    }

    /// Play Together, the host side: opens the `lan` session of the game that is starting and
    /// asks the party into it. The game starts whether or not this works; without a session it
    /// is a game like any other, and the launcher is told why with a `host_failed` notice.
    fn host_session(
        &mut self,
        id: u64,
        env: std::collections::BTreeMap<String, String>,
        host: control::Host,
        (title_id, title): (String, String),
        version: String,
        build: String,
    ) {
        let (Some(cloud), Some((endpoint, relay))) = (
            self.cloud.clone().filter(|_| self.me.is_some()),
            self.endpoint(),
        ) else {
            self.host_failed("The network is not ready to open a session.");
            self.ok(id, GameStarted { env, session: None });
            return;
        };
        let body = CreateSession {
            title_id,
            title,
            version,
            build,
            kind: SessionKind::Lan,
            max_members: host.max_members.clamp(2, anyplay_api::MAX_MEMBERS),
            visibility: host.visibility,
            properties: String::new(),
            endpoint,
            relay,
        };
        let party = host.party;
        // The game this session is for. The answer may come after that game ended or after
        // another one began; then the session belongs to no game and is left at once.
        let game_token = self.game.as_ref().map(|g| g.token.clone());
        self.spawn(
            async move { cloud.create_session(&body).await },
            move |service, result| match result {
                Ok(session)
                    if game_token.is_none()
                        || service.game.as_ref().map(|g| &g.token) != game_token.as_ref() =>
                {
                    service.leave_orphan(session.id);
                    service.ok(id, GameStarted { env, session: None });
                }
                Ok(session) if session.kind != SessionKind::Lan => {
                    service.host_failed("The cloud does not support Play Together yet.");
                    service.leave_orphan(session.id);
                    service.ok(id, GameStarted { env, session: None });
                }
                Ok(session) => {
                    let session_id = session.id.clone();
                    if let Some(game) = &mut service.game {
                        game.sessions.insert(session_id.clone());
                    }
                    service.session_changed(session);
                    service.sync_audience();
                    if party {
                        service.ask_party_into(&session_id);
                    }
                    service.ok(
                        id,
                        GameStarted {
                            env,
                            session: Some(session_id),
                        },
                    );
                }
                Err(error) => {
                    let failure = service.failure(&error);
                    service.host_failed(&failure.message);
                    service.ok(id, GameStarted { env, session: None });
                }
            },
        );
    }

    /// Leaves a session that was opened for a game that is gone.
    fn leave_orphan(&mut self, session: String) {
        if let Some(cloud) = self.cloud.clone() {
            self.spawn(async move { cloud.leave(&session).await }, |_, _| {});
        }
    }

    fn host_failed(&mut self, message: &str) {
        self.event(control::Event::Notice {
            code: "host_failed".into(),
            message: message.to_owned(),
        });
    }

    /// Asks the party of the player into `session`: the party moves in when the player leads it,
    /// else its members get an invitation each (the cloud sends it to friends only).
    fn ask_party_into(&mut self, session: &str) {
        let (Some(party), Some(cloud), Some(me)) = (
            self.party.clone(),
            self.cloud.clone(),
            self.me.as_ref().map(|m| m.id),
        ) else {
            return;
        };
        let session = session.to_owned();
        if party.leader == me {
            self.spawn(
                async move { cloud.move_party(&session).await },
                |_, result| {
                    if let Err(error) = result {
                        tracing::info!("the party could not be moved into the session: {error}");
                    }
                },
            );
        } else {
            let others: Vec<AccountId> = party
                .members
                .iter()
                .map(|m| m.account.id)
                .filter(|account| *account != me)
                .collect();
            if others.is_empty() {
                return;
            }
            self.spawn(
                async move { cloud.invite(&session, others).await },
                |_, result| {
                    if let Err(error) = result {
                        tracing::info!("the party could not be invited to the session: {error}");
                    }
                },
            );
        }
    }

    /// A session the cloud closed or took the player out of. A Play Together session that ends
    /// while the player's game still runs is told to the launcher, which says so.
    fn session_closed(&mut self, id: &str) {
        let me = self.me.as_ref().map(|m| m.id);
        let ended = self
            .sessions
            .get(id)
            .filter(|s| s.kind == SessionKind::Lan && Some(s.leader) != me)
            .filter(|_| self.game.as_ref().is_some_and(|g| g.sessions.contains(id)))
            .map(|s| control::Event::SessionEnded {
                session: s.id.clone(),
                title_id: s.title_id.clone(),
                title: s.title.clone(),
                host: s
                    .members
                    .iter()
                    .find(|m| m.account.id == s.leader)
                    .map(|m| m.account.clone())
                    .unwrap_or(anyplay_api::Account {
                        id: s.leader,
                        handle: String::new(),
                    }),
            });
        self.session_gone(id);
        if let Some(event) = ended {
            self.event(event);
        }
    }

    fn game_end(&mut self) {
        let Some(game) = self.game.take() else {
            return;
        };
        self.bridge_tokens.set(None);
        self.share_game_ended();
        if let Some(hub) = &self.screen.hub {
            hub.stop_sharing("game_ended");
        }
        self.screen_event();
        self.share_changed();
        if let Some(cloud) = self.cloud.clone() {
            for session in game.sessions {
                let cloud = cloud.clone();
                self.spawn(
                    async move { (cloud.leave(&session).await, session) },
                    |service, (_, session)| service.session_gone(&session),
                );
            }
        }
        self.traffic.unbind_all();
        self.sync_audience();
        if let Some(bridge) = &mut self.bridge {
            bridge.activated.clear();
        }
        self.activity = None;
        self.push_presence();
    }

    // The bridge

    /// Handles input from the bridge listener; true when the service should end now.
    fn bridge_input(&mut self, input: BridgeInput) -> bool {
        match input {
            BridgeInput::Hello {
                connection,
                frames,
                verdict,
            } => {
                // The listener has checked the bridge's proof of the token; the game may have
                // ended since.
                if self.game.is_none() {
                    let _ = verdict.send(Err("no game is starting".into()));
                    return false;
                }
                let welcome = Message::Welcome(Welcome {
                    version: bridge::VERSION,
                    udp_port: self.traffic.udp_port(),
                    tcp_port: self.traffic.tcp_port(),
                    stream_key: self.traffic.stream_key(),
                });
                if let Ok(frame) = Frame::event(&welcome) {
                    let _ = frames.send(frame);
                }
                let _ = verdict.send(Ok(()));
                tracing::info!("bridge {connection} connected");
                self.traffic.unbind_all();
                self.bridge = Some(BridgeLink {
                    connection,
                    frames,
                    activated: HashSet::new(),
                });
                self.last_bridge_state = None;
                self.share_changed();
                self.bridge_state();
                if let Some(game) = &mut self.game {
                    game.intent_sent = false;
                }
                self.send_intent();
            }
            BridgeInput::Frame { connection, frame } => {
                if self.bridge.as_ref().map(|b| b.connection) == Some(connection) {
                    self.bridge_frame(connection, frame);
                }
            }
            BridgeInput::Closed { connection } => {
                if self.bridge.as_ref().map(|b| b.connection) == Some(connection) {
                    tracing::info!("bridge {connection} disconnected");
                    self.bridge = None;
                    self.share_bridge_lost();
                    self.share_changed();
                    self.traffic.unbind_all();
                    self.dialogs.retain(|_, d| d.connection != connection);
                    if self.control_closed {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn bridge_event(&self, message: Message) {
        if let (Some(bridge), Ok(frame)) = (&self.bridge, Frame::event(&message)) {
            let _ = bridge.frames.send(frame);
        }
    }

    fn bridge_reply(&self, connection: u64, request: u32, message: Message) {
        let Some(bridge) = self.bridge.as_ref().filter(|b| b.connection == connection) else {
            return;
        };
        if let Ok(frame) = Frame::numbered(&message, request) {
            let _ = bridge.frames.send(frame);
        }
    }

    /// What the bridge should tell the game about sign-in right now.
    fn bridge_state_now(&self) -> bridge::State {
        let reachable = self.link == control::Cloud::Online
            || self
                .offline_since
                .is_some_and(|since| since.elapsed() < OFFLINE_GRACE);
        match &self.me {
            Some(me) if reachable => bridge::State {
                signed_in: true,
                account_id: me.id.0,
                handle: me.handle.clone(),
                nat_type: self.tunnels.as_ref().map_or(0, |tunnels| {
                    tunnel::nat_type(self.reach.as_ref(), tunnels.relay_connected())
                }),
                address: self.own_address(),
            },
            _ => bridge::State::signed_out(),
        }
    }

    fn bridge_state(&mut self) {
        if self.bridge.is_none() {
            return;
        }
        let state = self.bridge_state_now();
        if self.last_bridge_state.as_ref() == Some(&state) {
            return;
        }
        self.last_bridge_state = Some(state.clone());
        self.bridge_event(Message::State(state));
    }

    fn send_intent(&mut self) {
        let Some(game) = &mut self.game else { return };
        let (Some(session), false) = (game.join.clone(), game.intent_sent) else {
            return;
        };
        if self.bridge.is_none() {
            return;
        }
        game.intent_sent = true;
        self.bridge_event(Message::Intent(bridge::Intent {
            user_id: -1,
            intent_type: "joinSession".into(),
            properties: vec![("sessionId".into(), session)],
        }));
    }

    fn title(&self) -> String {
        self.game
            .as_ref()
            .map(|g| g.title_id.clone())
            .unwrap_or_default()
    }

    fn bridge_frame(&mut self, connection: u64, frame: Frame) {
        if frame.kind.is_reply() {
            return;
        }
        let message = match frame.message() {
            Ok(Some(message)) => message,
            Ok(None) | Err(_) => {
                if frame.request != 0
                    && let Some(bridge) = &self.bridge
                {
                    let _ = bridge.frames.send(frame.unsupported());
                }
                return;
            }
        };
        let request = frame.request;
        match message {
            Message::Feature { user_id, feature } => {
                self.observer.record(
                    &self.title(),
                    "feature",
                    serde_json::json!({"user_id": user_id, "feature": feature}),
                    serde_json::Value::Null,
                );
            }
            Message::WebRequest(web) => self.web_request(connection, request, web),
            Message::PushFilter(filter) => {
                self.observer.record(
                    &self.title(),
                    "push_filter",
                    serde_json::json!({"user_id": filter.user_id, "filter": filter.filter, "data_types": filter.data_types}),
                    serde_json::Value::Null,
                );
            }
            Message::PushFilterGone { .. } => {}
            Message::Activate(activate) => {
                let result = if self.me.is_none() {
                    ActivateResult::SignedOut
                } else if self.sessions.contains_key(&activate.session) {
                    ActivateResult::Done
                } else {
                    ActivateResult::UnknownSession
                };
                self.bridge_reply(connection, request, Message::Activated { result });
                if result == ActivateResult::Done {
                    if let Some(bridge) = &mut self.bridge {
                        bridge.activated.insert(activate.session.clone());
                    }
                    self.announce_peers(&activate.session);
                }
            }
            Message::Deactivate { session, .. } => {
                if let Some(bridge) = &mut self.bridge {
                    bridge.activated.remove(&session);
                }
            }
            Message::PeerInfo { address } => {
                let reply = match self.tunnels.as_ref().and_then(|t| t.link_of(address)) {
                    Some((link, rtt)) => PeerInfoReply {
                        known: true,
                        rtt_us: rtt
                            .map(|r| r.as_micros().min(u32::MAX as u128) as u32)
                            .unwrap_or(0),
                        relayed: link == Link::Relay,
                    },
                    None => PeerInfoReply {
                        known: false,
                        rtt_us: 0,
                        relayed: false,
                    },
                };
                self.bridge_reply(connection, request, Message::PeerInfoReply(reply));
            }
            Message::Bind(bind) => {
                let result = self
                    .traffic
                    .bind(bind.vport, bind.transport, bind.loopback_port);
                self.bridge_reply(connection, request, Message::Bound { result });
            }
            Message::Unbind { vport, transport } => {
                if transport != Transport::Unknown {
                    self.traffic.unbind(vport, transport);
                }
            }
            Message::InvitationDialog(dialog) => {
                let session = Some(dialog.session.clone())
                    .filter(|s| self.sessions.contains_key(s))
                    .or_else(|| self.current_session());
                let Some(session) = session.filter(|_| self.me.is_some()) else {
                    self.bridge_reply(
                        connection,
                        request,
                        Message::InvitationDialogReply {
                            result: DialogResult::Cancelled,
                            sent: 0,
                        },
                    );
                    return;
                };
                let Some(id) = admit_dialog(
                    &mut self.dialogs,
                    &mut self.next_dialog,
                    Dialog {
                        connection,
                        request,
                        session,
                    },
                ) else {
                    self.bridge_reply(
                        connection,
                        request,
                        Message::InvitationDialogReply {
                            result: DialogResult::Cancelled,
                            sent: 0,
                        },
                    );
                    return;
                };
                let title = self
                    .game
                    .as_ref()
                    .map(|g| g.title.clone())
                    .unwrap_or_default();
                self.event(control::Event::InviteDialog {
                    id,
                    title,
                    max: dialog.max,
                });
            }
            // The game's answers and news about a share go to the hub that holds it.
            Message::ShareOpened { .. }
            | Message::Override { .. }
            | Message::ShareStatus(_) => self.share_from_game(frame),
            Message::Hello(_)
            | Message::Welcome(_)
            | Message::Reject(_)
            | Message::Challenge(_)
            | Message::Auth(_)
            | Message::Unsupported
            | Message::State(_)
            | Message::WebReply(_)
            | Message::PushEvent(_)
            | Message::Activated { .. }
            | Message::Peer(_)
            | Message::PeerInfoReply(_)
            | Message::Bound { .. }
            | Message::Intent(_)
            | Message::InvitationDialogReply { .. }
            // The share kinds travel from the service to the game. Its host side is
            // `shareplay::ShareHost`; a game that sends one of them to a service that holds no
            // share is answered `unsupported` (a request) or ignored (an event).
            | Message::ShareOpen(_)
            | Message::ShareClose
            | Message::ShareMode { .. }
            | Message::SharePad(_)
            // The net service has no handler for the leaderboard requests yet: a game that sends
            // one is answered `unsupported`. Their replies travel from the service to the game.
            | Message::ScoreRecord(_)
            | Message::BoardRead(_)
            | Message::ScoreRecorded(_)
            | Message::BoardPage(_) => {
                if request != 0 {
                    self.bridge_reply(connection, request, Message::Unsupported);
                }
            }
        }
    }

    fn announce_peers(&self, session: &str) {
        let Some(session) = self.sessions.get(session) else {
            return;
        };
        let me = self.me.as_ref().map(|m| m.id);
        for member in &session.members {
            if Some(member.account.id) == me {
                continue;
            }
            let peer_state = match self.links.get(&member.account.id.0) {
                Some(Link::Direct | Link::Relay) => PeerState::Connected,
                _ => PeerState::Connecting,
            };
            self.bridge_event(Message::Peer(bridge::Peer {
                session: session.id.clone(),
                account_id: member.account.id.0,
                handle: member.account.handle.clone(),
                address: self.peer_address(member),
                peer_state,
            }));
        }
    }

    fn web_request(&mut self, connection: u64, request: u32, web: bridge::WebRequest) {
        if web.api_group == OWN_API_GROUP {
            self.own_api(connection, request, web);
            return;
        }
        let reply = self.offered_web_reply(&web);
        let mut input = serde_json::json!({
            "user_id": web.user_id,
            "api_group": web.api_group,
            "path": web.path,
            "method": web.method,
            "headers": observe::redact_headers(&web.headers),
            "body": observe::game_bytes(&web.body),
        });
        if web.body.len() > observe::CUT {
            input["cut"] = web.body.len().into();
        }
        self.observer.record(
            &self.title(),
            "web",
            input,
            serde_json::json!({"status": reply.status}),
        );
        self.bridge_reply(connection, request, Message::WebReply(reply));
    }

    /// The answer of a group the net service offers, or "not offered". The restrictions come
    /// from what the service already holds; a game polling many times a second causes a read
    /// from the cloud at most once per [`webapi::RESTRICTIONS_MAX_AGE`].
    fn offered_web_reply(&mut self, web: &bridge::WebRequest) -> WebReply {
        let stale = self
            .restrictions_asked
            .is_none_or(|asked| asked.elapsed() >= webapi::RESTRICTIONS_MAX_AGE);
        if stale && web.api_group == webapi::COMMUNICATION_RESTRICTION {
            self.fetch_restrictions();
        }
        let state = self.bridge_state_now();
        let overrides = std::env::var_os(webapi::OVERRIDES_VAR).map(PathBuf::from);
        let context = webapi::Context {
            account: state.signed_in.then_some(state.account_id),
            handle: &state.handle,
            friends: &self.friends,
            restrictions: &self.restrictions,
            now: now(),
            overrides: overrides.as_deref(),
        };
        webapi::answer(web, &context).unwrap_or_else(WebReply::not_offered)
    }

    /// The network's own API group: sessions in the cloud's format, for the fake game and for
    /// homebrew. Title and versions always come from the game start, never from the request.
    fn own_api(&mut self, connection: u64, request: u32, web: bridge::WebRequest) {
        let reply = move |service: &mut Service, status: u16, body: Vec<u8>| {
            service.bridge_reply(
                connection,
                request,
                Message::WebReply(WebReply {
                    status,
                    headers: vec![("Content-Type".into(), "application/json".into())],
                    body,
                }),
            );
        };
        let (Some(cloud), Some(game), Some((endpoint, relay))) = (
            self.cloud.clone().filter(|_| self.me.is_some()),
            self.game.as_ref().map(|g| {
                (
                    g.title_id.clone(),
                    g.title.clone(),
                    g.version.clone(),
                    g.build.clone(),
                )
            }),
            self.endpoint(),
        ) else {
            reply(self, 0, Vec::new());
            return;
        };
        let parts: Vec<&str> = web.path.trim_matches('/').split('/').collect();
        let method = web.method.to_ascii_uppercase();
        let finish = move |service: &mut Service, result: Result<Option<Session>, CloudError>| {
            match result {
                Ok(Some(session)) => {
                    let body = serde_json::to_vec(&session).unwrap_or_default();
                    service.session_changed(session);
                    reply(service, 200, body);
                }
                Ok(None) => reply(service, 204, Vec::new()),
                Err(CloudError::Api(error)) => {
                    let body = serde_json::to_vec(&error).unwrap_or_default();
                    reply(service, error.error.status(), body);
                }
                Err(_) => reply(service, 0, Vec::new()),
            }
        };
        // A game's own words never steer the cloud path: every segment is a plain id or a fixed word.
        if parts.iter().any(|part| !anyplay_api::is_title_id(part)) {
            reply(self, 400, Vec::new());
            return;
        }
        match (method.as_str(), parts.as_slice()) {
            ("POST", ["v1", "sessions"]) => {
                #[derive(serde::Deserialize)]
                struct Create {
                    max_members: u16,
                    visibility: Visibility,
                    #[serde(default)]
                    properties: String,
                }
                let Ok(create) = serde_json::from_slice::<Create>(&web.body) else {
                    reply(self, 400, Vec::new());
                    return;
                };
                let (title_id, title, version, build) = game;
                let body = CreateSession {
                    title_id,
                    title,
                    version,
                    build,
                    kind: SessionKind::Game,
                    max_members: create.max_members,
                    visibility: create.visibility,
                    properties: create.properties,
                    endpoint,
                    relay,
                };
                self.spawn(
                    async move { cloud.create_session(&body).await.map(Some) },
                    move |service, result| {
                        if let (Ok(Some(session)), Some(game)) = (&result, &mut service.game) {
                            game.sessions.insert(session.id.clone());
                        }
                        service.sync_audience();
                        finish(service, result);
                    },
                );
            }
            ("POST", ["v1", "sessions", id, "join"]) => {
                let id = (*id).to_owned();
                let (_, _, version, build) = game;
                let body = JoinSession {
                    version,
                    build,
                    endpoint,
                    relay,
                    invitation: None,
                    // A game's own join calls are the game's: exact, as before.
                    version_rule: VersionRule::Exact,
                    override_version: false,
                };
                self.spawn(
                    async move { cloud.join(&id, &body).await.map(Some) },
                    move |service, result| {
                        if let (Ok(Some(session)), Some(game)) = (&result, &mut service.game) {
                            game.sessions.insert(session.id.clone());
                        }
                        service.sync_audience();
                        finish(service, result);
                    },
                );
            }
            ("POST", ["v1", "sessions", id, "leave"]) => {
                let id = (*id).to_owned();
                self.spawn(
                    async move { (cloud.leave(&id).await, id) },
                    move |service, (result, id)| {
                        if result.is_ok() {
                            service.session_gone(&id);
                        }
                        finish(service, result.map(|()| None));
                    },
                );
            }
            ("GET", ["v1", "sessions", id]) => {
                let id = (*id).to_owned();
                self.spawn(async move { cloud.session(&id).await.map(Some) }, finish);
            }
            _ => reply(self, 404, Vec::new()),
        }
    }

    // Timers and state

    fn tick(&mut self) {
        if self.friends_dirty && self.friends_sent.elapsed() >= FRIENDS_PACE {
            self.friends_dirty = false;
            self.friends_sent = Instant::now();
            self.event(control::Event::Friends(self.friends.clone()));
        }
        if self.feed_dirty && self.feed_sent.elapsed() >= FEED_PACE {
            self.feed_dirty = false;
            self.feed_sent = Instant::now();
            self.event(control::Event::Feed);
        }
        if self.me.is_some() && self.relays_expire != 0 && self.relays_expire < now() + 3600 {
            self.ask_relays();
        }
        if self.me.is_some() && self.tunnels.is_none() && !self.tunnels_starting {
            self.ask_relays();
        }
        self.bridge_state();
    }

    /// Takes the account's appear-offline setting from the cloud and tells the launcher.
    fn set_appear_offline(&mut self, on: bool) {
        if let Some(me) = self.me.as_mut()
            && me.appear_offline != on
        {
            me.appear_offline = on;
            self.state_changed();
        }
    }

    fn state_changed(&mut self) {
        let account = self.me.as_ref().map(|me| SignedIn {
            id: me.id,
            handle: me.handle.clone(),
            email: me.email.clone(),
            appear_offline: me.appear_offline,
        });
        let pending = match self.pending {
            Some(PendingSignIn::Code { .. }) => Some(Pending::Code),
            Some(PendingSignIn::Handle { .. }) => Some(Pending::Handle),
            Some(PendingSignIn::Browser) => Some(Pending::Browser),
            None => None,
        };
        let state = (self.link, account, pending);
        if self.last_state.as_ref() != Some(&state) {
            self.last_state = Some(state.clone());
            let (cloud, account, pending) = state;
            self.event(control::Event::State {
                cloud,
                account,
                pending,
            });
        }
        self.bridge_state();
    }

    /// Leaves the sessions of a running game and closes the tunnels before the process ends.
    async fn goodbye(&mut self) {
        let sessions: Vec<String> = self
            .game
            .as_ref()
            .map(|g| g.sessions.iter().cloned().collect())
            .unwrap_or_default();
        let cloud = self.cloud.clone();
        let tunnels = self.tunnels.take();
        let presence = self.me.is_some() && self.activity.is_some();
        let _ = tokio::time::timeout(GOODBYE, async move {
            if let Some(cloud) = cloud {
                for session in sessions {
                    let _ = cloud.leave(&session).await;
                }
                if presence {
                    let _ = cloud.presence(&PresenceRequest { activity: None }).await;
                }
            }
            if let Some(tunnels) = tunnels {
                tunnels.close().await;
            }
        })
        .await;
        if let Some((_, task)) = self.events.take() {
            task.abort();
        }
    }
}

/// Reads command lines until the launcher's stream ends.
async fn read_commands<R>(input: R, inbox: UnboundedSender<Input>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::AsyncBufReadExt;
    let mut reader = tokio::io::BufReader::new(input);
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = (&mut reader)
            .take(control::MAX_LINE as u64)
            .read_until(b'\n', &mut line)
            .await;
        match read {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.last() != Some(&b'\n') && line.len() >= control::MAX_LINE {
            tracing::warn!("a line from the launcher is too long; skipping it");
            // Throw the rest of the line away piece by piece, never holding it.
            let mut ended = false;
            loop {
                line.clear();
                match (&mut reader)
                    .take(control::MAX_LINE as u64)
                    .read_until(b'\n', &mut line)
                    .await
                {
                    Ok(0) | Err(_) => break,
                    Ok(_) if line.last() == Some(&b'\n') => {
                        ended = true;
                        break;
                    }
                    Ok(_) => {}
                }
            }
            if !ended {
                break;
            }
            continue;
        }
        let Ok(text) = std::str::from_utf8(&line) else {
            let _ = inbox.send(Input::BadCommand(None));
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        let input = match control::parse_command(text.trim_end()) {
            Ok((id, command)) => Input::Command(id, command),
            Err(id) => Input::BadCommand(id),
        };
        if inbox.send(input).is_err() {
            return;
        }
    }
    let _ = inbox.send(Input::ControlClosed);
}

async fn write_lines<W>(mut output: W, mut lines: UnboundedReceiver<String>)
where
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    use tokio::io::AsyncWriteExt;
    while let Some(line) = lines.recv().await {
        if output.write_all(line.as_bytes()).await.is_err() {
            return;
        }
        while let Ok(more) = lines.try_recv() {
            if output.write_all(more.as_bytes()).await.is_err() {
                return;
            }
        }
        if output.flush().await.is_err() {
            return;
        }
    }
}

/// The names of the microphones and speakers; listing may block, so it runs off the loop.
async fn list_devices(audio: Audio) -> (Vec<String>, Vec<String>) {
    tokio::task::spawn_blocking(move || (audio.input_names(), audio.output_names()))
        .await
        .unwrap_or_default()
}

/// The members of `party` other than the player (account `me`, tunnel key `mine`) that voice may
/// connect to, by the tunnel key the party names for each. A member who has announced no readable
/// key is not in it: nobody can be reached, and nobody may connect, without one. A key that two
/// members name is in it for neither.
fn members_of(party: &anyplay_api::Party, me: AccountId, mine: EndpointId) -> voice::Members {
    let keys: Vec<Option<EndpointId>> = party
        .members
        .iter()
        .map(|m| m.endpoint.parse::<EndpointId>().ok())
        .collect();
    party
        .members
        .iter()
        .zip(&keys)
        .filter(|(m, _)| m.account.id != me)
        .filter_map(|(m, endpoint)| {
            let endpoint = (*endpoint)?;
            // A key two members name belongs to neither of them: whose voice it is cannot be told.
            let claimed = keys.iter().filter(|k| **k == Some(endpoint)).count();
            (endpoint != mine && claimed == 1).then(|| {
                (
                    endpoint,
                    voice::Member {
                        account: m.account.id.0,
                        relay: m.relay.as_deref().and_then(|r| r.parse::<RelayUrl>().ok()),
                        voice: m.voice,
                    },
                )
            })
        })
        .collect()
}

/// A party as the launcher sees it: without the members' tunnel keys and relays, which are for
/// the net services of the party alone.
fn launcher_party(mut party: anyplay_api::Party) -> anyplay_api::Party {
    for member in &mut party.members {
        member.endpoint.clear();
        member.relay = None;
    }
    party
}

fn bad_message_text() -> Failure {
    Failure::new(
        ErrorCode::BadRequest.as_str(),
        "A message has 1 to 2000 characters and is not only white space.",
    )
}

fn bad_language() -> Failure {
    Failure::new(
        ErrorCode::BadRequest.as_str(),
        "A language is two or three lower-case letters, such as en or de.",
    )
}

/// Reads a screenshot or clip for an upload: the `Content-Type` by the file's first bytes, and
/// the bytes. Refuses a file that is not a PNG, JPEG or MP4, or is larger than the cloud takes,
/// before anything is sent.
async fn read_media(path: std::path::PathBuf) -> Result<(&'static str, Vec<u8>), CloudError> {
    let refuse =
        |message: &str| CloudError::Api(anyplay_api::ApiError::new(ErrorCode::BadRequest, message));
    tokio::task::spawn_blocking(move || {
        use std::io::Read as _;
        let mut file =
            std::fs::File::open(&path).map_err(|_| refuse("The file cannot be opened."))?;
        let size = file
            .metadata()
            .map_err(|_| refuse("The file cannot be read."))?
            .len();
        let mut head = [0u8; 12];
        let got = file.read(&mut head).unwrap_or(0);
        let content_type = media_type(&head[..got])
            .ok_or_else(|| refuse("Only PNG and JPEG screenshots and MP4 clips can be sent."))?;
        let kind = anyplay_api::MediaKind::from_content_type(content_type)
            .ok_or_else(|| refuse("Only PNG and JPEG screenshots and MP4 clips can be sent."))?;
        if size == 0 || size > kind.max_bytes() {
            return Err(CloudError::Api(anyplay_api::ApiError::new(
                ErrorCode::TooLarge,
                "A screenshot may be 10 MB, a clip 100 MB.",
            )));
        }
        let mut bytes = Vec::with_capacity(size as usize);
        bytes.extend_from_slice(&head[..got]);
        file.read_to_end(&mut bytes)
            .map_err(|_| refuse("The file cannot be read."))?;
        Ok((content_type, bytes))
    })
    .await
    .map_err(|_| CloudError::Offline("the file could not be read".into()))?
}

/// The `Content-Type` of a screenshot or clip by its first bytes.
fn media_type(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("image/png")
    } else if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if head.len() >= 8 && &head[4..8] == b"ftyp" {
        Some("video/mp4")
    } else {
        None
    }
}

fn bad_id() -> Failure {
    Failure::new(
        ErrorCode::BadRequest.as_str(),
        "An ID has 1 to 64 letters, digits, underscores and hyphens.",
    )
}

fn bad_title() -> Failure {
    Failure::new(
        ErrorCode::BadRequest.as_str(),
        "A title ID has 1 to 64 letters, digits, underscores and hyphens.",
    )
}

fn bad_run_version() -> Failure {
    Failure::new(
        ErrorCode::BadRequest.as_str(),
        "A version has 1 to 32 letters, digits, dots, underscores, plus signs and hyphens.",
    )
}

/// The system this service runs on, as a run report names it.
fn this_os() -> Option<anyplay_api::Os> {
    match std::env::consts::OS {
        "windows" => Some(anyplay_api::Os::Windows),
        "linux" => Some(anyplay_api::Os::Linux),
        "macos" => Some(anyplay_api::Os::Macos),
        _ => None,
    }
}

/// What is wrong with playtime entries, if anything, before they go to the cloud.
fn playtime_problem(entries: &[PlaytimeEntry]) -> Option<String> {
    if entries.len() > MAX_PLAYTIME_UPLOAD {
        return Some(format!(
            "Send at most {MAX_PLAYTIME_UPLOAD} entries at a time."
        ));
    }
    for entry in entries {
        if !is_title_id(&entry.title_id) {
            return Some(bad_title().message);
        }
        if day_number(&entry.day).is_none() {
            return Some("A day is a date such as 2026-10-24.".into());
        }
        if entry.minutes > MAX_DAY_MINUTES {
            return Some("A day has 1,440 minutes.".into());
        }
    }
    None
}

/// The endpoint key of this device, made on first use and kept in `data`.
fn endpoint_key(data: &std::path::Path) -> SecretKey {
    let path = data.join("endpoint.key");
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(key) = text.trim().parse::<SecretKey>() {
            return key;
        }
        tracing::warn!("the endpoint key is unreadable; making a new one");
    }
    let key = SecretKey::generate();
    let text = data_encoding::HEXLOWER.encode(&key.to_bytes());
    // Owner-only and through a temporary file, like the sign-in: the key is this device's identity.
    if let Err(error) = crate::credentials::write_private(&path, &text) {
        tracing::warn!("could not keep the endpoint key: {error}");
    }
    key
}

#[cfg(test)]
mod once_tests {
    use super::*;

    #[test]
    fn once_is_first_only_once() {
        let mut once = Once::default();
        assert!(once.first());
        assert!(!once.first());
        assert!(!once.first());
    }
}

#[cfg(test)]
mod profile_tests {
    use super::*;

    fn entry(title: &str, day: &str, minutes: u32) -> PlaytimeEntry {
        PlaytimeEntry {
            title_id: title.into(),
            day: day.into(),
            minutes,
        }
    }

    #[test]
    fn playtime_is_checked_before_it_leaves() {
        assert_eq!(
            playtime_problem(&[entry("GAME00001", "2026-10-24", 1440)]),
            None
        );
        assert!(playtime_problem(&[entry("GAME00001", "2026-10-24", 1441)]).is_some());
        assert!(playtime_problem(&[entry("GAME00001", "24.10.2026", 5)]).is_some());
        assert!(playtime_problem(&[entry("../x", "2026-10-24", 5)]).is_some());
        let many = vec![entry("GAME00001", "2026-10-24", 1); MAX_PLAYTIME_UPLOAD + 1];
        assert!(playtime_problem(&many).is_some());
    }
}

#[cfg(test)]
mod party_voice_tests {
    use super::*;
    use anyplay_api::{Account, Party, PartyMember};

    fn member(id: u64, endpoint: &str, relay: Option<&str>, voice: bool) -> PartyMember {
        PartyMember {
            account: Account {
                id: AccountId(id),
                handle: format!("p{id}"),
            },
            joined: 0,
            endpoint: endpoint.into(),
            relay: relay.map(str::to_owned),
            voice,
        }
    }

    fn party(members: Vec<PartyMember>) -> Party {
        Party {
            id: "p".into(),
            leader: AccountId(1),
            members,
            max_members: 8,
            conversation: "c".into(),
            created: 0,
            revision: 1,
            discord: None,
        }
    }

    #[test]
    fn voice_may_reach_only_the_other_members_who_named_a_key() {
        let mine = SecretKey::generate().public();
        let a = SecretKey::generate().public();
        let b = SecretKey::generate().public();
        let twice = SecretKey::generate().public();
        let party = party(vec![
            member(1, &mine.to_string(), None, true),
            member(2, &a.to_string(), Some("https://relay.example/"), true),
            member(3, &b.to_string(), None, false),
            member(4, "", None, true),
            member(5, "not a key", None, true),
            // another account claiming this device's key is not a peer either
            member(6, &mine.to_string(), None, true),
            // two accounts naming one key: whose voice it is cannot be told
            member(7, &twice.to_string(), None, true),
            member(8, &twice.to_string(), None, true),
        ]);
        let members = members_of(&party, AccountId(1), mine);
        assert_eq!(members.len(), 2);
        assert_eq!(members[&a].account, 2);
        assert!(members[&a].voice);
        assert!(members[&a].relay.is_some());
        assert_eq!(members[&b].account, 3);
        assert!(!members[&b].voice);
    }

    #[test]
    fn the_launcher_never_sees_tunnel_keys() {
        let key = SecretKey::generate().public().to_string();
        let seen = launcher_party(party(vec![member(
            2,
            &key,
            Some("https://r.example/"),
            true,
        )]));
        assert_eq!(seen.members[0].endpoint, "");
        assert_eq!(seen.members[0].relay, None);
        assert!(seen.members[0].voice, "who takes part is for the launcher");
    }
}

#[cfg(test)]
mod hardening_tests {
    use super::*;

    #[test]
    fn an_endpoint_named_by_two_accounts_or_by_us_gets_no_peer() {
        let own = SecretKey::generate().public();
        let shared = SecretKey::generate().public();
        let alone = SecretKey::generate().public();
        let named = vec![
            (shared, 2, "first"),
            (alone, 3, "alone"),
            (shared, 4, "second"),
            (own, 5, "us"),
            // one account in two sessions is one peer
            (alone, 3, "alone-again"),
        ];
        let peers = unambiguous_endpoints(named, own);
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[&alone], "alone");
    }

    #[test]
    fn an_endpoint_the_cloud_keeps_refusing_is_pushed_once() {
        let mut pushed = HashMap::new();
        let relay = Some("https://r.example/".to_owned());
        assert!(endpoint_push_due(&mut pushed, "s", true, "e", &relay));
        // still stale after the push: the cloud answered differently, do not ask again
        for _ in 0..5 {
            assert!(!endpoint_push_due(&mut pushed, "s", true, "e", &relay));
        }
        // a new relay is news
        let other = Some("https://r2.example/".to_owned());
        assert!(endpoint_push_due(&mut pushed, "s", true, "e", &other));
        // once the record matches, the memory is cleared and a later change pushes again
        assert!(!endpoint_push_due(&mut pushed, "s", false, "e", &other));
        assert!(endpoint_push_due(&mut pushed, "s", true, "e", &other));
    }

    #[test]
    fn a_join_answer_counts_only_for_the_game_that_asked() {
        let run = |token: u8| GameRun {
            title_id: String::new(),
            title: String::new(),
            version: String::new(),
            build: String::new(),
            token: vec![token; 32],
            join: None,
            intent_sent: false,
            sessions: HashSet::new(),
        };
        assert!(game_is(Some(&run(1)), &[1; 32]));
        assert!(!game_is(Some(&run(2)), &[1; 32]), "another game began");
        assert!(!game_is(None, &[1; 32]), "the game ended");
    }

    #[test]
    fn at_most_eight_invitation_dialogs_wait() {
        let mut dialogs = HashMap::new();
        let mut next = 1;
        let make = |n: u32| Dialog {
            connection: 1,
            request: n,
            session: "s".into(),
        };
        for n in 0..MAX_DIALOGS as u32 {
            assert!(admit_dialog(&mut dialogs, &mut next, make(n)).is_some());
        }
        assert!(admit_dialog(&mut dialogs, &mut next, make(99)).is_none());
        assert_eq!(dialogs.len(), MAX_DIALOGS);
        dialogs.remove(&1);
        assert!(admit_dialog(&mut dialogs, &mut next, make(100)).is_some());
    }

    #[tokio::test]
    async fn a_line_without_end_is_dropped_and_the_next_line_is_read() {
        let mut bytes = vec![b'x'; control::MAX_LINE * 5 + 17];
        bytes.push(b'\n');
        bytes.extend_from_slice(b"{\"id\":4,\"do\":\"join_code_revoke\"}\n");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        read_commands(std::io::Cursor::new(bytes), tx).await;
        let first = rx.recv().await.unwrap();
        assert!(matches!(first, Input::Command(4, _)));
        assert!(matches!(rx.recv().await.unwrap(), Input::ControlClosed));
    }

    #[test]
    fn the_endpoint_key_is_kept_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let first = endpoint_key(dir.path());
        let second = endpoint_key(dir.path());
        assert_eq!(first.to_bytes(), second.to_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("endpoint.key"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "the key is for the owner only");
        }
    }
}
