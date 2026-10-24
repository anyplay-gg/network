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
    Activity, CodeAnswer, CreateSession, Device, ErrorCode, Event, FriendLists, Invitation,
    JoinSession, Me, Platform, PresenceRequest, Report, Session, Visibility, check_handle,
};
use anyplay_bridge_protocol::{
    self as bridge, ActivateResult, DialogResult, Frame, Message, PeerInfoReply, PeerState,
    Transport, WebReply, Welcome,
};
use anyplay_control::{
    self as control, Accepted, Answer, CodeAccepted, CodeResult, Command, Done, EmailSent, Failure,
    FriendAdded, GameStarted, Hello, Invited, Output, Pending, SignedIn,
};
use iroh::{EndpointId, RelayUrl, SecretKey};
use tokio::io::AsyncReadExt as _;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use crate::bridge::BridgeInput;
use crate::cloud::{Cloud, CloudError, now};
use crate::credentials::{Saved, Store};
use crate::events::{self, EventInput};
use crate::observe::{self, Observer};
use crate::traffic::Traffic;
use crate::tunnel::{self, Link, LinkChange, RelayAccess, Tunnels};

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
}

type Continuation = Box<dyn FnOnce(&mut Service) + Send>;

enum Input {
    Command(u64, Command),
    BadCommand(Option<u64>),
    ControlClosed,
    Events(u64, EventInput),
    Bridge(BridgeInput),
    Link(LinkChange),
    Done(Continuation),
    Tick,
    Stop,
}

/// How long the player stays signed in for the game while the cloud cannot be reached.
const OFFLINE_GRACE: Duration = Duration::from_secs(30);
/// How often the launcher hears about friends at most.
const FRIENDS_PACE: Duration = Duration::from_millis(250);
/// How long a quitting service waits for its last requests to the cloud.
const GOODBYE: Duration = Duration::from_secs(3);
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

enum PendingSignIn {
    Code { challenge: String },
    Handle { signup: String },
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
    link: control::Cloud,
    offline_since: Option<Instant>,
    events: Option<(u64, JoinHandle<()>)>,
    next_events: u64,
    address: Option<Ipv4Addr>,
    friends: FriendLists,
    friends_dirty: bool,
    friends_sent: Instant,
    sessions: HashMap<String, Session>,
    invitations: HashMap<String, Invitation>,
    activity: Option<Activity>,
    game: Option<GameRun>,
    bridge: Option<BridgeLink>,
    bridge_port: u16,
    dialogs: HashMap<u64, Dialog>,
    next_dialog: u64,
    traffic: Traffic,
    secret: SecretKey,
    tunnels: Option<Tunnels>,
    tunnels_starting: bool,
    relays_expire: u64,
    relays_asked: Option<Instant>,
    links: HashMap<u64, Link>,
    observer: Observer,
    last_state: Option<(control::Cloud, Option<SignedIn>, Option<Pending>)>,
    last_bridge_state: Option<bridge::State>,
    control_closed: bool,
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
        let bridge_port = crate::bridge::listen(bridge_tx).await?;
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
        let service = Self {
            config,
            inbox: tx,
            out,
            hello: false,
            cloud: None,
            store: None,
            me: None,
            pending: None,
            link: control::Cloud::Offline,
            offline_since: None,
            events: None,
            next_events: 1,
            address: None,
            friends: FriendLists::default(),
            friends_dirty: false,
            friends_sent: Instant::now() - FRIENDS_PACE,
            sessions: HashMap::new(),
            invitations: HashMap::new(),
            activity: None,
            game: None,
            bridge: None,
            bridge_port,
            dialogs: HashMap::new(),
            next_dialog: 1,
            traffic,
            secret,
            tunnels: None,
            tunnels_starting: false,
            relays_expire: 0,
            relays_asked: None,
            links: HashMap::new(),
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
                if api.error == ErrorCode::Banned {
                    self.end_online("banned");
                }
                Failure::api(api)
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
        }
    }

    // Commands

    fn command(&mut self, id: u64, command: Command) {
        if !self.hello && !matches!(command, Command::Hello { .. }) {
            self.fail(id, Failure::new(Failure::BAD_STATE, "Say hello first."));
            return;
        }
        match command {
            Command::Hello { launcher, cloud } => self.hello(id, &launcher, &cloud),
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
            } => {
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
                            })
                            .await
                    },
                    |_, ()| Done {},
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
                            } else if found.version != version || found.build != build {
                                let mut failure = Failure::new(
                                    ErrorCode::VersionMismatch.as_str(),
                                    format!(
                                        "Your friend plays version {} with AnyPS5 {}.",
                                        found.version, found.build
                                    ),
                                );
                                failure.expected = Some(anyplay_api::Expected {
                                    version: found.version,
                                    build: found.build,
                                });
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
            } => self.game_start(id, title_id, title, version, build, join),
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
        let sink_store = store.clone();
        let sink: crate::cloud::TokenSink = Arc::new(move |tokens| {
            let saved = tokens.map(|t| Saved {
                device: t.device,
                refresh: t.refresh.clone(),
            });
            let store = sink_store.clone();
            std::thread::spawn(move || store.save(saved.as_ref()));
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
                cloud.resume(saved.device, saved.refresh);
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
        self.me = None;
        self.pending = None;
        self.link = control::Cloud::Offline;
        self.offline_since = None;
        self.address = None;
        self.friends = FriendLists::default();
        self.friends_dirty = true;
        self.sessions.clear();
        self.invitations.clear();
        self.links.clear();
        if let Some(tunnels) = self.tunnels.take() {
            tokio::spawn(async move { tunnels.close().await });
        }
        self.relays_expire = 0;
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
            EventInput::Banned(until) => {
                let message = match until {
                    Some(until) => format!("This account is banned until {until} (Unix time)."),
                    None => "This account is banned.".to_owned(),
                };
                self.event(control::Event::Notice {
                    code: ErrorCode::Banned.as_str().into(),
                    message,
                });
                self.end_online("banned");
            }
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
            Event::Session { session } => self.session_changed(session),
            Event::SessionGone { id } => self.session_gone(&id),
            Event::SignedOut | Event::Banned { .. } | Event::Unknown => {}
        }
    }

    fn fetch_friends(&mut self) {
        let Some(cloud) = self.cloud.clone().filter(|_| self.me.is_some()) else {
            return;
        };
        self.spawn(
            async move { cloud.friends().await },
            |service, result| match result {
                Ok(lists) => {
                    service.friends = lists;
                    service.friends_dirty = true;
                }
                Err(CloudError::Offline(_)) => {}
                Err(error) => {
                    service.failure(&error);
                }
            },
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
            self.spawn(
                async move { (cloud.session(&id).await, id) },
                |service, (result, id)| match result {
                    Ok(session) => service.session_changed(session),
                    Err(CloudError::Api(error)) if error.error == ErrorCode::NotFound => {
                        service.session_gone(&id)
                    }
                    Err(_) => {}
                },
            );
        }
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
        let options = tunnel::Options {
            secret: self.secret.clone(),
            relays,
            relay_roots: self.config.relay_roots.clone(),
            bind: self.config.bind,
            relay_only: self.config.relay_only,
        };
        let traffic = self.traffic.clone();
        self.spawn(
            async move {
                let tunnels = Tunnels::start(options, traffic, changes).await?;
                tunnels.online(Duration::from_secs(10)).await;
                Ok::<_, String>(tunnels)
            },
            |service, result| {
                service.tunnels_starting = false;
                match result {
                    Ok(tunnels) if service.me.is_some() => {
                        tracing::info!("tunnel endpoint {}", tunnels.id());
                        service.tunnels = Some(tunnels);
                        service.update_peers();
                        service.update_endpoints();
                        service.bridge_state();
                    }
                    Ok(tunnels) => {
                        tokio::spawn(async move { tunnels.close().await });
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
        for session in self.sessions.values() {
            let stale = session
                .members
                .iter()
                .find(|m| m.account.id == me)
                .is_some_and(|m| m.endpoint != endpoint || m.relay != relay);
            if !stale {
                continue;
            }
            let cloud = cloud.clone();
            let id = session.id.clone();
            let body = JoinSession {
                version: session.version.clone(),
                build: session.build.clone(),
                endpoint: endpoint.clone(),
                relay: relay.clone(),
                invitation: None,
            };
            self.spawn(
                async move { cloud.join(&id, &body).await },
                |service, result| {
                    if let Ok(session) = result {
                        service.session_changed(session);
                    }
                },
            );
        }
    }

    fn update_peers(&mut self) {
        let Some(tunnels) = &self.tunnels else {
            return;
        };
        let me = self.me.as_ref().map(|m| m.id);
        let mut peers = HashMap::new();
        for session in self.sessions.values() {
            for member in &session.members {
                if Some(member.account.id) == me {
                    continue;
                }
                let (Ok(endpoint), Ok(address)) = (
                    member.endpoint.parse::<EndpointId>(),
                    member.address.parse::<Ipv4Addr>(),
                ) else {
                    continue;
                };
                peers.insert(
                    endpoint,
                    tunnel::Peer {
                        account: member.account.id.0,
                        handle: member.account.handle.clone(),
                        address,
                        relay: member
                            .relay
                            .as_deref()
                            .and_then(|r| r.parse::<RelayUrl>().ok()),
                    },
                );
            }
        }
        tunnels.set_peers(peers);
    }

    fn link_change(&mut self, change: LinkChange) {
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
                    address: member.address.parse().unwrap_or(Ipv4Addr::UNSPECIFIED),
                    peer_state: state,
                }));
            }
        }
        self.session_view();
    }

    // Sessions

    fn session_changed(&mut self, session: Session) {
        let id = session.id.clone();
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
            if joined {
                self.announce_peers(&id);
            }
        }
        self.update_peers();
        self.update_endpoints();
        self.session_view();
    }

    fn session_gone(&mut self, id: &str) {
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
        self.session_view();
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
                address: member.address.parse().unwrap_or(Ipv4Addr::UNSPECIFIED),
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
                        address: member.address.clone(),
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
    ) {
        if self.game.is_some() {
            self.game_end();
        }
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
        let Some(session) = join else {
            self.ok(id, GameStarted { env });
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
        };
        self.spawn(
            async move { cloud.join(&session, &body).await },
            move |service, result| match result {
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
                    service.ok(id, GameStarted { env });
                }
                Err(error) => {
                    let failure = service.failure(&error);
                    service.fail(id, failure);
                }
            },
        );
    }

    fn game_end(&mut self) {
        let Some(game) = self.game.take() else {
            return;
        };
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
                version,
                token,
                frames,
                verdict,
            } => {
                let Some(game) = &self.game else {
                    let _ = verdict.send(Err("no game is starting".into()));
                    return false;
                };
                if !crate::traffic::same(&token, &game.token) {
                    let _ = verdict.send(Err("the token does not match this game start".into()));
                    return false;
                }
                if version != bridge::VERSION {
                    tracing::info!(
                        "bridge speaks version {version}, answering with {}",
                        bridge::VERSION
                    );
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
                nat_type: if self.tunnels.is_some() { 2 } else { 0 },
                address: self.address.unwrap_or(Ipv4Addr::UNSPECIFIED),
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
                let id = self.next_dialog;
                self.next_dialog += 1;
                self.dialogs.insert(
                    id,
                    Dialog {
                        connection,
                        request,
                        session,
                    },
                );
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
            Message::Hello(_)
            | Message::Welcome(_)
            | Message::Reject(_)
            | Message::Unsupported
            | Message::State(_)
            | Message::WebReply(_)
            | Message::PushEvent(_)
            | Message::Activated { .. }
            | Message::Peer(_)
            | Message::PeerInfoReply(_)
            | Message::Bound { .. }
            | Message::Intent(_)
            | Message::InvitationDialogReply { .. } => {
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
                address: member.address.parse().unwrap_or(Ipv4Addr::UNSPECIFIED),
                peer_state,
            }));
        }
    }

    fn web_request(&mut self, connection: u64, request: u32, web: bridge::WebRequest) {
        if web.api_group == OWN_API_GROUP {
            self.own_api(connection, request, web);
            return;
        }
        let mut input = serde_json::json!({
            "user_id": web.user_id,
            "api_group": web.api_group,
            "path": web.path,
            "method": web.method,
            "headers": web.headers.iter().map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone()))).collect::<serde_json::Map<_, _>>(),
            "body": observe::game_bytes(&web.body),
        });
        if web.body.len() > observe::CUT {
            input["cut"] = web.body.len().into();
        }
        self.observer.record(
            &self.title(),
            "web",
            input,
            serde_json::json!({"status": 0}),
        );
        self.bridge_reply(
            connection,
            request,
            Message::WebReply(WebReply::not_offered()),
        );
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
                };
                self.spawn(
                    async move { cloud.join(&id, &body).await.map(Some) },
                    move |service, result| {
                        if let (Ok(Some(session)), Some(game)) = (&result, &mut service.game) {
                            game.sessions.insert(session.id.clone());
                        }
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
        if self.me.is_some() && self.relays_expire != 0 && self.relays_expire < now() + 3600 {
            self.ask_relays();
        }
        if self.me.is_some() && self.tunnels.is_none() && !self.tunnels_starting {
            self.ask_relays();
        }
        self.bridge_state();
    }

    fn state_changed(&mut self) {
        let account = self.me.as_ref().map(|me| SignedIn {
            id: me.id,
            handle: me.handle.clone(),
            email: me.email.clone(),
        });
        let pending = match self.pending {
            Some(PendingSignIn::Code { .. }) => Some(Pending::Code),
            Some(PendingSignIn::Handle { .. }) => Some(Pending::Handle),
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
            let mut rest = Vec::new();
            if reader.read_until(b'\n', &mut rest).await.unwrap_or(0) == 0 {
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
    let _ = std::fs::create_dir_all(data);
    let text = data_encoding::HEXLOWER.encode(&key.to_bytes());
    if let Err(error) = std::fs::write(&path, text) {
        tracing::warn!("could not keep the endpoint key: {error}");
    }
    key
}
