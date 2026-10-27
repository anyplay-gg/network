//! Messages between the launcher and the net service it starts.
//!
//! The protocol is specified in `spec/control-protocol.md`: JSON lines over the net service's
//! standard streams. The launcher has no async runtime, so this crate is plain data and line
//! handling; it does no input or output itself.

use std::collections::BTreeMap;

use anyplay_api::{
    Account, AccountId, ErrorCode, Expected, FriendLists, Invitation, VersionRule, Visibility,
};
use serde::{Deserialize, Serialize};

pub use anyplay_api;

/// The protocol version this crate speaks.
pub const VERSION: u16 = 0;

/// The longest line either side writes or reads, newline included.
pub const MAX_LINE: usize = 1024 * 1024;

/// A command from the launcher, without its number.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "do", rename_all = "snake_case")]
pub enum Command {
    Hello {
        launcher: String,
        cloud: String,
    },
    /// Deprecated: use `browser_sign_in`. Still works on a cloud without an account web.
    Email {
        email: String,
    },
    /// Deprecated: use `browser_sign_in`.
    Code {
        code: String,
    },
    /// Deprecated: use `browser_sign_in` with `create`.
    SignUp {
        handle: String,
        age_confirmed: bool,
    },
    SignOut,
    DeleteAccount,
    /// What the net service knows of Discord now (phase 0 of ADR 0022).
    DiscordStatus,
    /// Connects the player's Discord account through the SDK: with a refresh token the service
    /// kept for this account, else Discord asks the player. Answers at once; the `discord` event
    /// follows.
    DiscordConnect,
    /// Disconnects and forgets the kept refresh token.
    DiscordDisconnect,
    AddFriend {
        handle: String,
    },
    Accept {
        account: AccountId,
    },
    Decline {
        account: AccountId,
    },
    RemoveFriend {
        account: AccountId,
    },
    Block {
        account: AccountId,
    },
    Unblock {
        account: AccountId,
    },
    Report {
        account: AccountId,
        reason: anyplay_api::ReportReason,
        #[serde(default)]
        details: String,
        /// With `seq`: the message being reported, in a conversation the player can read.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conversation: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<u64>,
        /// The looking-for-group listing being reported.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        listing: Option<String>,
    },
    Playing {
        game: Option<Game>,
    },
    CheckJoin {
        session: String,
        title_id: String,
        version: String,
        build: String,
        /// How far `version` may differ from the session's (the launcher's table of games).
        #[serde(default, skip_serializing_if = "is_exact")]
        version_rule: VersionRule,
        /// "Try Anyway": the player chose to join despite the rule. The build is still checked.
        #[serde(default, skip_serializing_if = "is_false")]
        override_version: bool,
    },
    GameStart {
        title_id: String,
        title: String,
        version: String,
        build: String,
        join: Option<String>,
        /// Play Together: open a `lan` session for this game before it starts, and ask the
        /// party into it. Without `join` only; with both, `join` wins.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host: Option<Host>,
        /// With `join`: the rule and the override, passed on to the cloud's join as they are.
        #[serde(default, skip_serializing_if = "is_exact")]
        version_rule: VersionRule,
        #[serde(default, skip_serializing_if = "is_false")]
        override_version: bool,
    },
    GameEnd,
    Invite {
        accounts: Vec<AccountId>,
    },
    // A command's own fields never take the name `id`: that is the command's number, and a second
    // `id` on the line would replace it. These send the invitation or dialog under its own name.
    AnswerInvitation {
        #[serde(rename = "invitation")]
        id: String,
        accept: bool,
    },
    Dialog {
        #[serde(rename = "dialog")]
        id: u64,
        accounts: Option<Vec<AccountId>>,
    },
    RelayOnly {
        on: bool,
    },
    /// Makes the account appear offline to every other account, or shows it again. Answers
    /// `{appear_offline}`; the `state` event follows.
    SetAppearOffline {
        on: bool,
    },
    UploadTrophies {
        title_id: String,
        trophies: Vec<anyplay_api::Trophy>,
    },
    Trophies {
        account: AccountId,
    },
    Rarity {
        title_id: String,
    },
    /// The player's trophies in one title next to another account's: the cloud's
    /// `TrophyCompare`. `not_found` when that account's trophies are hidden from the player.
    Compare {
        account: AccountId,
        title_id: String,
    },
    /// Marks trophies of a title as secret: the game hides them until they are revealed.
    /// Answers `{changed}`.
    SecretTrophies {
        title_id: String,
        numbers: Vec<u16>,
    },
    /// Reveals secret trophies of a title to everyone who may see the player's trophies.
    /// Answers `{changed}`.
    RevealTrophies {
        title_id: String,
        numbers: Vec<u16>,
    },
    /// How a game ran; the net service adds the player's system.
    RunReport {
        title_id: String,
        version: String,
        toolkit: String,
        verdict: anyplay_api::Verdict,
    },
    /// What players found; `version` asks for that game version first.
    RunReports {
        title_id: String,
        version: Option<String>,
    },
    Cards {
        accounts: Vec<AccountId>,
    },
    Privacy,
    SetPrivacy {
        privacy: anyplay_api::Privacy,
    },
    /// Whether the player's page at `/u/<handle>` is shown to everyone.
    PublicProfile,
    /// `{on}`: shows the page to everyone, or only to the player and friends again.
    SetPublicProfile {
        on: bool,
    },
    Profile {
        account: AccountId,
    },
    UploadPlaytime {
        entries: Vec<anyplay_api::PlaytimeEntry>,
    },
    /// A page of the feed: the player's own items and their friends', newest first. `before`
    /// is a `next` of an earlier page; `limit` is at most 50.
    Feed {
        #[serde(default)]
        before: Option<u64>,
        #[serde(default)]
        limit: Option<u32>,
    },
    /// Sets the player's reaction to a feed item, or takes it back when `reaction` is null.
    /// (The item is named `item`: `id` is the number of the command.)
    React {
        item: u64,
        #[serde(default)]
        reaction: Option<anyplay_api::Reaction>,
    },
    // Parties and messages
    /// The player's party, or null.
    Party,
    /// Makes a party of one with the player as its leader, or returns the one they are in.
    PartyCreate,
    /// Invites friends to the party; makes one when the player is in none.
    PartyInvite {
        accounts: Vec<AccountId>,
        /// Ring them: a call invitation that lives 30 seconds.
        #[serde(default)]
        call: bool,
    },
    /// The calls nobody answered.
    MissedCalls,
    /// Marks the missed calls of one caller (or of all, when `account` is null) as seen.
    CallsSeen {
        #[serde(default)]
        account: Option<AccountId>,
    },
    /// Sets the party's Discord link (the leader); the empty string removes it.
    PartyLink {
        url: String,
    },
    /// Mutes or unmutes the player's own microphone in party voice.
    MicMute {
        muted: bool,
    },
    /// Stops (or resumes) hearing the whole party. Deafened also silences the microphone.
    Deafen {
        deafened: bool,
    },
    PartyAnswer {
        #[serde(rename = "invitation")]
        id: String,
        accept: bool,
    },
    PartyLeave,
    PartyKick {
        account: AccountId,
    },
    PartyLeader {
        account: AccountId,
    },
    /// The leader invites the party into the session the player is in.
    PartyMove {
        session: String,
    },
    // Join codes
    /// A new join code for the player (the cloud revokes the earlier one).
    JoinCode,
    JoinCodeRevoke,
    /// Turns a code, or a link that holds one, into an invitation.
    JoinRedeem {
        code: String,
    },
    // Voice
    /// Switches party voice on or off. The microphone is only opened while it is on and the
    /// player is in a party.
    Voice {
        on: bool,
    },
    /// Push-to-talk: whether the key is down.
    PushToTalk {
        talking: bool,
    },
    /// Noise suppression on the microphone: on (the default of a new service) or off.
    VoiceNoise {
        enabled: bool,
    },
    VoiceMode {
        mode: VoiceMode,
    },
    /// How loud one member is heard, in percent: 100 is as sent, 0 to 200.
    PeerVolume {
        account: AccountId,
        percent: u16,
    },
    PeerMute {
        account: AccountId,
        muted: bool,
    },
    /// The names of the microphones and speakers, and which are chosen.
    VoiceDevices,
    /// Chooses a microphone or speakers by name; null is the system's default.
    SetVoiceDevice {
        kind: DeviceKind,
        name: Option<String>,
    },
    // Share screen
    /// Starts streaming the game the player is playing to the party's members, who each choose
    /// whether to watch. Watch only: no input comes back.
    ScreenStart {
        #[serde(default)]
        source: ScreenSource,
        /// The game's window (a Windows `HWND` as a number), if the launcher has found it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window: Option<i64>,
        /// The processes of the game: the one the launcher started and what it started.
        /// Without a window or processes the share worker finds the window by the game's
        /// title, which can pick a wrong one.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pids: Vec<u32>,
    },
    /// Ends the stream the player shares.
    ScreenStop,
    /// Watches the stream a party member shares (one at a time).
    ScreenWatch {
        account: AccountId,
    },
    /// Stops watching.
    ScreenLeave,
    /// Tells the net service where the verified share worker is.
    ScreenWorker {
        path: String,
    },
    // Share play
    /// The host invites a member of their party to share play: the guest sees the game, hears
    /// it, and depending on `mode` plays. Needs a running game and a worker that can encode.
    ShareStart {
        account: AccountId,
        mode: anyplay_api::ShareMode,
    },
    /// The guest answers an invitation (`share_invitation`): accepting opens the viewer.
    ShareAnswer {
        /// The invitation's id (named `invitation` on the wire: `id` is the command's number).
        #[serde(rename = "invitation")]
        id: String,
        accept: bool,
    },
    /// The host changes the mode of the running share.
    ShareMode {
        mode: anyplay_api::ShareMode,
    },
    /// The guest asks the host for another mode; nothing changes until the host agrees.
    ShareRequest {
        mode: anyplay_api::ShareMode,
    },
    /// Ends the share: the host stops it, the guest leaves it, or an invitation is declined.
    ShareStop,
    Conversations,
    /// Opens the conversation with one account, or a group with several.
    OpenConversation {
        accounts: Vec<AccountId>,
        #[serde(default)]
        name: Option<String>,
    },
    Messages {
        conversation: String,
        #[serde(default)]
        after: Option<u64>,
        #[serde(default)]
        before: Option<u64>,
        #[serde(default)]
        limit: Option<u32>,
        /// The messages that changed after this `rev`, in the order of the change. Not with
        /// `after` or `before`.
        #[serde(default)]
        changed_after: Option<u64>,
    },
    SendMessage {
        conversation: String,
        text: String,
        /// The number of the message this one answers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply: Option<u64>,
    },
    /// Changes the text of the player's own message, within 15 minutes of writing it.
    EditMessage {
        conversation: String,
        seq: u64,
        text: String,
    },
    /// Removes the text of the player's own message for everyone.
    UnsendMessage {
        conversation: String,
        seq: u64,
    },
    /// Sets the player's reaction to a message; `reaction` null takes it back.
    ReactMessage {
        conversation: String,
        seq: u64,
        reaction: Option<anyplay_api::MessageReaction>,
    },
    /// The player types in the conversation (`typing` true), or stopped. The net service sends
    /// at most one `true` every three seconds per conversation and drops the rest.
    Typing {
        conversation: String,
        typing: bool,
    },
    /// Mutes or pins a conversation for the player. A field left out stays.
    ConversationSettings {
        conversation: String,
        #[serde(default)]
        muted: Option<bool>,
        #[serde(default)]
        pinned: Option<bool>,
    },
    MarkRead {
        conversation: String,
        seq: u64,
    },
    AcceptRequest {
        conversation: String,
    },
    LeaveConversation {
        conversation: String,
    },
    AddToConversation {
        conversation: String,
        accounts: Vec<AccountId>,
    },
    /// Sends a screenshot (PNG, JPEG) or a clip (MP4) from a file on this PC to a friend's
    /// conversation. Answers the `Message` once the upload is done; a clip can take minutes. The
    /// service checks the size and the file's type before it sends anything.
    SendMedia {
        conversation: String,
        path: String,
    },
    /// Downloads the file of a media message into `path`. Answers `Done` when the file is whole;
    /// nothing is left at `path` on a failure.
    FetchMedia {
        media: String,
        path: String,
    },
    // Looking for group
    /// The open listings of one game, newest first. `mode` and `language` narrow them.
    Lfg {
        title_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<anyplay_api::LfgMode>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<String>,
    },
    /// The player's own listings with their requests, and the listings they asked to join.
    LfgMine,
    /// Posts a listing for 1 hour.
    LfgPost {
        title_id: String,
        mode: anyplay_api::LfgMode,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<String>,
        #[serde(default)]
        note: String,
        slots: u8,
    },
    /// Closes the player's own listing.
    LfgClose {
        listing: String,
    },
    /// Asks to join someone's listing.
    LfgAsk {
        listing: String,
    },
    /// Takes the player's request back.
    LfgWithdraw {
        listing: String,
    },
    /// The poster's answer to a request: accepting puts the player into the poster's party by
    /// invitation, declining removes the request without telling them.
    LfgAnswer {
        listing: String,
        account: AccountId,
        accept: bool,
    },
    /// Starts signing in (or creating an account) in the system browser. Answers the URL for the
    /// launcher to open; the end comes as a `state` event with the account.
    BrowserSignIn {
        #[serde(default)]
        create: bool,
    },
    /// Ends a browser sign-in that is waiting.
    CancelSignIn,
    /// Answers a short-lived link that opens the account web, signed in as this account.
    AccountPage,
    /// Files the one appeal against an action that mutes the account. A ban is appealed on the
    /// account web instead, because a banned account is signed out.
    Appeal {
        action: u64,
        text: String,
    },
}

/// When the microphone is open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceMode {
    /// While the push-to-talk key is down. The default.
    #[default]
    PushToTalk,
    /// Whenever speech is heard. Without headphones the others hear their own voices back.
    VoiceActivity,
}

/// Which sound device a command means.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    Input,
    Output,
}

/// The game a player is playing, as the launcher knows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Game {
    pub title_id: String,
    pub title: String,
    pub version: String,
}

#[derive(Serialize)]
struct CommandOut<'a> {
    id: u64,
    #[serde(flatten)]
    command: &'a Command,
}

#[derive(Deserialize)]
struct CommandHead {
    id: u64,
}

fn is_exact(rule: &VersionRule) -> bool {
    *rule == VersionRule::Exact
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// The line that sends `command` as number `id`, newline included.
pub fn command_line(id: u64, command: &Command) -> String {
    line(&CommandOut { id, command })
}

/// Reads a command line. An unreadable command still yields its number when it has one, so the
/// net service can answer `unknown_command`.
pub fn parse_command(line: &str) -> Result<(u64, Command), Option<u64>> {
    match serde_json::from_str::<serde_json::Value>(line) {
        Ok(value) => {
            let id = CommandHead::deserialize(&value).ok().map(|head| head.id);
            match (id, Command::deserialize(value)) {
                (Some(id), Ok(command)) => Ok((id, command)),
                (id, _) => Err(id),
            }
        }
        Err(_) => Err(None),
    }
}

/// An error answer. `code` is a cloud error code or one of the control protocol's own.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<Box<Expected>>,
    /// With the codes `restricted` and `banned`: what is restricted and until when.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restriction: Option<Box<Restriction>>,
    /// With the code `client_too_old`: the oldest launcher version the cloud accepts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum: Option<String>,
}

/// What limits an account, as the player is told. Plain strings and numbers, like the code of a
/// `Failure`, so that a launcher still reads a value a newer cloud adds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Restriction {
    /// `muted` (no friend requests and no messages), `new_account` (too new to write to
    /// strangers or to send more friend requests) or `banned`. Treat any other value as a
    /// restriction you cannot name.
    pub kind: String,
    /// Unix seconds when it ends. Absent for a ban for good.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<u64>,
    /// The rule of the terms (`harassment`, `spam` ...) and the moderator's words. Absent for
    /// `new_account`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The number of the action, to appeal it with `appeal`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<u64>,
    /// Where the appeal stands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub appeal: Option<Appeal>,
}

/// Where the appeal of an action stands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Appeal {
    /// `available`, `open`, `upheld`, `reversed` or `expired`.
    pub status: String,
    /// Unix seconds. An appeal can be filed until then.
    pub deadline: u64,
}

impl Restriction {
    pub const MUTED: &'static str = "muted";
    pub const NEW_ACCOUNT: &'static str = "new_account";
    pub const BANNED: &'static str = "banned";

    /// A restriction without a reason, such as a new account's.
    pub fn new(kind: impl Into<String>, until: Option<u64>) -> Self {
        Self {
            kind: kind.into(),
            until,
            rule: None,
            reason: None,
            action: None,
            appeal: None,
        }
    }

    /// The restriction a mute or a ban states; none for a warning.
    pub fn of_statement(statement: &anyplay_api::Statement) -> Option<Self> {
        let kind = match statement.kind {
            anyplay_api::ActionKind::Warning => return None,
            anyplay_api::ActionKind::Mute => Self::MUTED,
            anyplay_api::ActionKind::Ban => Self::BANNED,
        };
        Some(Self {
            kind: kind.to_owned(),
            until: statement.until,
            rule: Some(statement.rule.as_str().to_owned()),
            reason: Some(statement.reason.clone()),
            action: Some(statement.id),
            appeal: Some(Appeal {
                status: statement.appeal.status.as_str().to_owned(),
                deadline: statement.appeal.deadline,
            }),
        })
    }

    /// The restriction a ban states, from its end and statement.
    pub fn of_ban(until: Option<u64>, statement: Option<&anyplay_api::Statement>) -> Self {
        let mut restriction = statement
            .and_then(Self::of_statement)
            .unwrap_or_else(|| Self::new(Self::BANNED, None));
        restriction.kind = Self::BANNED.to_owned();
        restriction.until = restriction.until.or(until);
        restriction
    }

    /// The restriction a cloud error states: `restricted` or `banned`.
    pub fn of_error(error: &anyplay_api::ApiError) -> Option<Self> {
        match error.error {
            ErrorCode::Restricted => Some(Self::new(error.restriction?.as_str(), error.until)),
            ErrorCode::Banned => Some(Self::of_ban(
                error.until,
                error.ban.as_ref().and_then(|ban| ban.statement.as_ref()),
            )),
            _ => None,
        }
    }
}

impl Failure {
    pub const UNKNOWN_COMMAND: &'static str = "unknown_command";
    pub const OFFLINE: &'static str = "offline";
    pub const NOT_SIGNED_IN: &'static str = "not_signed_in";
    pub const NO_GAME: &'static str = "no_game";
    pub const BAD_STATE: &'static str = "bad_state";
    pub const NOT_IN_PARTY: &'static str = "not_in_party";
    pub const NO_ENCODER: &'static str = "no_encoder";
    pub const NO_DECODER: &'static str = "no_decoder";
    pub const BUSY: &'static str = "busy";
    /// The cloud has share play switched off.
    pub const NOT_ALLOWED: &'static str = "not_allowed";

    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            expected: None,
            restriction: None,
            minimum: None,
        }
    }

    /// The failure that carries a cloud error.
    pub fn api(error: &anyplay_api::ApiError) -> Self {
        Self {
            code: error.error.as_str().to_owned(),
            message: error.message.clone(),
            expected: error.expected.clone(),
            restriction: Restriction::of_error(error).map(Box::new),
            minimum: error.minimum.as_deref().map(str::to_owned),
        }
    }

    /// True when the failure is this cloud error.
    pub fn is(&self, code: ErrorCode) -> bool {
        self.code == code.as_str()
    }
}

/// Everything the net service writes on its standard output.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Output {
    Answer(Answer),
    Event(Event),
}

/// The answer to command number `id`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    pub id: u64,
    #[serde(flatten)]
    pub outcome: Outcome,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok(serde_json::Value),
    Error(Failure),
}

impl Answer {
    pub fn ok(id: u64, value: impl Serialize) -> Self {
        Self {
            id,
            outcome: Outcome::Ok(serde_json::to_value(value).unwrap_or_default()),
        }
    }

    pub fn error(id: u64, failure: Failure) -> Self {
        Self {
            id,
            outcome: Outcome::Error(failure),
        }
    }

    /// The `ok` value read as `T`, or the failure.
    pub fn into_result<T: serde::de::DeserializeOwned>(self) -> Result<T, Failure> {
        match self.outcome {
            Outcome::Ok(value) => serde_json::from_value(value)
                .map_err(|e| Failure::new(Failure::BAD_STATE, format!("unreadable answer: {e}"))),
            Outcome::Error(failure) => Err(failure),
        }
    }
}

/// The line that carries `output`, newline included.
pub fn output_line(output: &Output) -> String {
    line(output)
}

/// Reads one line of the net service's output. `None` for a line that is neither an answer nor
/// an event this version understands the outline of.
pub fn parse_output(line: &str) -> Option<Output> {
    serde_json::from_str(line).ok()
}

fn line(value: &impl Serialize) -> String {
    let mut text = serde_json::to_string(value).unwrap_or_default();
    text.push('\n');
    text
}

// Answers

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub service: String,
    pub protocol: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmailSent {
    pub code_length: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeResult {
    SignedIn,
    NewAccount,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeAccepted {
    pub result: CodeResult,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendAdded {
    pub result: anyplay_api::FriendState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameStarted {
    pub env: BTreeMap<String, String>,
    /// The `lan` session the start opened (`host` was given and the cloud made it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

/// What a launcher asks for to host a game: a Play Together session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Host {
    /// The most players, the host included.
    pub max_members: u16,
    /// `friends` lets a friend of any member join from presence; `invite` only the invited.
    pub visibility: Visibility,
    /// Ask the host's party into the session when it opens: the party moves in when the host
    /// leads it, else its friends among the host's party get an invitation.
    #[serde(default)]
    pub party: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invited {
    pub sent: Vec<AccountId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accepted {
    pub session: String,
    pub title_id: String,
}

/// A page for the launcher to open in the system browser.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Opened {
    pub url: String,
}

/// Where the player is with Discord.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscordState {
    /// This build has no Discord SDK, or the library did not load.
    Unavailable,
    SignedOut,
    Connecting,
    Connected,
}

/// The player's Discord account while connected. `id` is a decimal string.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscordUser {
    pub id: String,
    pub name: String,
}

/// The answer to `discord_status`, and the fields of the `discord` event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscordInfo {
    pub state: DiscordState,
    pub user: Option<DiscordUser>,
    /// The number of Discord friends while connected; their names stay in the net service.
    pub friends: Option<u32>,
}

/// An answer without fields.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Done {}

/// What a shared screen shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenSource {
    /// The window of the game the player is playing, and nothing else. The default.
    #[default]
    Game,
    /// A moving test picture with a frame counter in its pixels, for tests and for measuring.
    TestPattern,
}

/// How far a shared screen has come, on either side.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenPhase {
    /// The worker or the connection is starting.
    Connecting,
    /// Pictures flow.
    Live,
    /// Connected, but nothing has arrived for 5 seconds.
    Stalled,
}

/// What a stream looks like at the moment: zero where not known yet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenStats {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub kbps: u32,
}

/// One party member as the host's stream sees them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenViewer {
    pub account: AccountId,
    /// The member chose to watch; otherwise they only know the stream is on.
    pub watching: bool,
    /// The connection to this member and the path it takes; null when there is none.
    pub link: Option<Link>,
    /// Round trip on that connection in milliseconds, when measured.
    pub rtt_ms: Option<u32>,
    /// Packets lost on the way to this member over the last seconds, in percent.
    pub loss_percent: u8,
}

/// The stream the player shares.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenHosting {
    pub phase: ScreenPhase,
    pub title: String,
    pub codec: Option<String>,
    #[serde(flatten)]
    pub stats: ScreenStats,
    pub viewers: Vec<ScreenViewer>,
}

/// A party member who shares a stream the player may watch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenOffer {
    pub account: AccountId,
    pub title: String,
    pub link: Option<Link>,
}

/// The stream the player watches.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenWatching {
    pub account: AccountId,
    pub title: String,
    pub phase: ScreenPhase,
    pub link: Option<Link>,
    pub rtt_ms: Option<u32>,
    #[serde(flatten)]
    pub stats: ScreenStats,
    /// Pictures shown per second.
    pub shown_fps: u32,
}

/// Which side of a share the player is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareRole {
    Host,
    Guest,
}

/// How far a share has come.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharePhase {
    /// The host invited the guest and waits for the answer.
    Invited,
    /// Accepted; the connection, the worker or the game's picture is not there yet.
    Connecting,
    /// Pictures flow.
    Live,
    /// Connected, but nothing has arrived for 5 seconds.
    Stalled,
}

/// The numbers of a share about twice a second: zero or null where not known yet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareStats {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub kbps: u32,
    pub rtt_ms: Option<u32>,
    pub loss_percent: u8,
}

/// The share the player is in, as the launcher shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareView {
    pub id: String,
    pub role: ShareRole,
    /// The other side: the guest for the host, the host for the guest.
    pub peer: Account,
    pub title: String,
    pub mode: anyplay_api::ShareMode,
    pub phase: SharePhase,
    /// The connection to the peer and the path it takes; null before there is one.
    pub link: Option<Link>,
    pub codec: Option<String>,
    #[serde(flatten)]
    pub stats: ShareStats,
    /// The guest asked for this mode and the host has not answered (host only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested: Option<anyplay_api::ShareMode>,
}

/// The answer to `screen_worker`: what the worker found it can do.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenWorkerReady {
    pub encoders: Vec<ScreenCodec>,
    pub decoders: Vec<ScreenCodec>,
    pub capture: bool,
    /// The codec libraries the worker loads while it runs (OpenH264 on Linux) and how each
    /// stands; empty where it loads none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub libraries: Vec<ScreenLibrary>,
}

/// A codec library the share worker loads, and whether it could.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenLibrary {
    /// Which library: `openh264`.
    pub name: String,
    /// `ready`, `off` (the player turned it off), `missing` (not installed), `refused` (a file
    /// that is not a known build) or `failed` (a file that could not be read, or a known build
    /// that would not load).
    pub state: String,
    /// More about the state when there is more to say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One codec a worker can encode or decode.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenCodec {
    pub codec: String,
    pub hardware: bool,
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: u32,
}

/// The answer to `party` and `party_answer`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartyState {
    pub party: Option<anyplay_api::Party>,
}

/// The answer to `voice_devices`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceDevices {
    pub input: Vec<String>,
    pub output: Vec<String>,
    /// The chosen microphone; null is the system's default.
    pub selected_input: Option<String>,
    pub selected_output: Option<String>,
}

// Events

/// Where party voice is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceState {
    /// Switched off, or the player is in no party, or no sound device could be opened.
    #[default]
    Off,
    /// Switched on in a party; the devices are opening, or members who take part are not
    /// reached yet.
    Connecting,
    /// The devices run and the members who take part are reached (or there are none).
    On,
}

/// How loud someone is and whether to light them up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceLevel {
    /// Recent loudness in percent, 0 to 100, before the player's volume setting.
    pub level: u8,
    pub speaking: bool,
}

/// One other member in the `voice` event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoicePeer {
    pub account: AccountId,
    #[serde(flatten)]
    pub level: VoiceLevel,
    /// The voice connection to this member and the path it takes; null when there is none.
    pub link: Option<Link>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cloud {
    Offline,
    Connecting,
    Online,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pending {
    Code,
    Handle,
    /// The browser sign-in is open; the net service waits for it.
    Browser,
}

/// The signed-in account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedIn {
    pub id: AccountId,
    pub handle: String,
    pub email: String,
    /// The account appears offline to others; the player still sees themselves as online.
    #[serde(default)]
    pub appear_offline: bool,
}

/// How the tunnel to another member runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Link {
    Connecting,
    Direct,
    Relay,
}

/// A member of the player's session as the launcher shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub account: Account,
    pub address: String,
    pub joined: u64,
    /// The tunnel to this member; null for the player.
    pub link: Option<Link>,
}

/// The player's session as the launcher shows it: the cloud's session without tunnel keys and
/// properties, with the state of each tunnel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    #[serde(default)]
    pub kind: anyplay_api::SessionKind,
    pub title_id: String,
    pub title: String,
    pub version: String,
    pub leader: AccountId,
    pub max_members: u16,
    pub visibility: Visibility,
    pub members: Vec<Member>,
    pub created: u64,
}

/// Something the net service reports on its own.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    State {
        cloud: Cloud,
        account: Option<SignedIn>,
        pending: Option<Pending>,
    },
    Friends(FriendLists),
    /// The state of the Discord connection changed (and once after `hello`).
    Discord(DiscordInfo),
    Invitation {
        invitation: Invitation,
    },
    InvitationGone {
        id: String,
    },
    Session {
        session: Option<Session>,
    },
    InviteDialog {
        id: u64,
        title: String,
        max: u16,
    },
    /// A Play Together session ended without the player leaving it: its host left or was
    /// disconnected for good. The player's game goes on; it has no one to play with.
    SessionEnded {
        session: String,
        title_id: String,
        title: String,
        host: Account,
    },
    Notice {
        code: String,
        message: String,
    },
    /// What limits the account now: its mutes that have not ended or been lifted. Sent after
    /// sign-in, after a moderator acts and after an appeal is decided; an empty list means none.
    /// A new account's limits are not here: they come with the `restricted` failure.
    Restrictions {
        restrictions: Vec<Restriction>,
    },
    /// The account was banned and is signed out. A `notice` with the code `banned` comes too.
    Banned {
        restriction: Restriction,
    },
    /// The cloud refuses this launcher because it is older than `minimum`. Sent once per run of
    /// the net service; the service stays offline until it is restarted.
    ClientTooOld {
        minimum: String,
    },
    // Parties and messages
    /// The player's party changed, began (a party) or ended (null).
    Party {
        party: Option<anyplay_api::Party>,
    },
    PartyInvitation {
        invitation: anyplay_api::PartyInvitation,
    },
    PartyInvitationGone {
        id: String,
    },
    /// Every call nobody answered, after sign-in and after every reconnect.
    MissedCalls(anyplay_api::MissedCalls),
    /// A friend called and the player did not answer.
    MissedCall {
        call: anyplay_api::MissedCall,
    },
    /// A missed call was marked seen (on another device, or by `calls_seen`).
    MissedCallGone {
        account: AccountId,
    },
    /// A call the player placed ended unanswered.
    CallEnded {
        account: AccountId,
        reason: anyplay_api::CallEnd,
    },
    /// Party voice: its state, and while it runs who is speaking. Sent when the state, `enabled`
    /// or the mode changes, and at most ten times a second while a level changes.
    Voice {
        state: VoiceState,
        /// The player switched voice on (with `voice`). It stays on across parties.
        enabled: bool,
        mode: VoiceMode,
        /// The player's microphone is muted.
        #[serde(default)]
        muted: bool,
        /// The player hears nobody.
        #[serde(default)]
        deafened: bool,
        me: VoiceLevel,
        peers: Vec<VoicePeer>,
    },
    /// Share screen: what the player shares, who offers a stream to them, and what they watch.
    /// Sent when something changes, and while a stream runs its numbers at most twice a second.
    Screen {
        /// The player could start a stream now: signed in, in a party, a game runs, and the
        /// worker can capture and encode.
        can_share: bool,
        /// The player could watch a stream: the worker can decode and show.
        can_watch: bool,
        hosting: Option<ScreenHosting>,
        offers: Vec<ScreenOffer>,
        watching: Option<ScreenWatching>,
    },
    /// Share play: the share the player is in (null when none) and whether they could start
    /// one now. Sent when something changes, and while a share runs its numbers at most twice
    /// a second.
    Share {
        /// Signed in, in a party with someone else, a game runs, the worker can capture and
        /// encode, and the cloud offers share play.
        can_start: bool,
        share: Option<ShareView>,
    },
    /// A party member invited the player to share play. Answer with `share_answer`.
    ShareInvitation {
        invitation: anyplay_api::ShareInvitation,
    },
    /// An invitation expired, was answered or its share ended.
    ShareInvitationGone {
        id: String,
    },
    /// The guest asks the host for another mode (host only). Answer with `share_mode` or ignore.
    ShareRequest {
        mode: anyplay_api::ShareMode,
    },
    /// Every conversation and request with its newest number, after sign-in and after every
    /// reconnect. Compare each `last` with the number you hold and fetch the messages after it.
    Conversations(anyplay_api::Conversations),
    /// A new message, by number; fetch it with `messages`.
    Message {
        conversation: String,
        seq: u64,
    },
    MessageRequest {
        conversation: String,
    },
    Read {
        conversation: String,
        account: AccountId,
        seq: u64,
    },
    /// A member's device received a direct conversation up to `seq`.
    Delivered {
        conversation: String,
        account: AccountId,
        seq: u64,
    },
    /// A member types in a conversation, or stopped. Drop it after six seconds.
    Typing {
        conversation: String,
        account: AccountId,
        typing: bool,
    },
    /// A message was edited, unsent or reacted to; fetch `messages` with `changed_after`.
    MessageUpdate {
        conversation: String,
        seq: u64,
        rev: u64,
    },
    /// A conversation's members, name, mute or pin changed, or it is new; fetch `conversations`.
    Conversation {
        id: String,
    },
    ConversationGone {
        id: String,
    },
    /// Someone asked to join a listing of the player; fetch `lfg_mine`.
    LfgRequest {
        listing: String,
        title_id: String,
        from: Account,
    },
    /// The poster of a listing the player asked to join accepted; their party invitation arrives
    /// as `party_invitation`.
    LfgAccepted {
        listing: String,
        title_id: String,
        by: Account,
    },
    /// The feed has news; fetch `feed`. Sent at most once every two seconds.
    Feed,
    /// An event this version does not know.
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_join_commands_are_lines_of_their_own() {
        assert_eq!(
            command_line(3, &Command::JoinCode),
            "{\"id\":3,\"do\":\"join_code\"}\n"
        );
        assert_eq!(
            parse_command(r#"{"id":4,"do":"join_code_revoke"}"#),
            Ok((4, Command::JoinCodeRevoke))
        );
        let redeem = Command::JoinRedeem {
            code: "anyps5://v1/join/k3x9m2pqra".into(),
        };
        let line = command_line(5, &redeem);
        assert_eq!(
            line,
            "{\"id\":5,\"do\":\"join_redeem\",\"code\":\"anyps5://v1/join/k3x9m2pqra\"}\n"
        );
        assert_eq!(parse_command(line.trim_end()), Ok((5, redeem)));
    }

    #[test]
    fn the_discord_commands_and_event_have_their_wire_form() {
        assert_eq!(
            command_line(3, &Command::DiscordConnect),
            "{\"id\":3,\"do\":\"discord_connect\"}\n"
        );
        assert_eq!(
            parse_command("{\"id\":4,\"do\":\"discord_status\"}"),
            Ok((4, Command::DiscordStatus))
        );
        let event = Event::Discord(DiscordInfo {
            state: DiscordState::Connected,
            user: Some(DiscordUser {
                id: "1563714917998133289".into(),
                name: "Lumen".into(),
            }),
            friends: Some(3),
        });
        let text = serde_json::to_string(&event).unwrap();
        assert_eq!(
            text,
            "{\"event\":\"discord\",\"state\":\"connected\",\"user\":{\"id\":\"1563714917998133289\",\"name\":\"Lumen\"},\"friends\":3}"
        );
        assert_eq!(serde_json::from_str::<Event>(&text).unwrap(), event);
    }

    #[test]
    fn a_command_is_one_line_with_its_number_and_name() {
        let line = command_line(
            7,
            &Command::AddFriend {
                handle: "Lumen".into(),
            },
        );
        assert_eq!(
            line,
            "{\"id\":7,\"do\":\"add_friend\",\"handle\":\"Lumen\"}\n"
        );
        assert_eq!(
            parse_command(line.trim_end()),
            Ok((
                7,
                Command::AddFriend {
                    handle: "Lumen".into()
                }
            ))
        );
        assert_eq!(
            parse_command(&command_line(1, &Command::SignOut)),
            Ok((1, Command::SignOut))
        );
        assert_eq!(
            parse_command(r#"{"id":2,"do":"relay_only","on":true}"#),
            Ok((2, Command::RelayOnly { on: true }))
        );
        assert_eq!(
            parse_command(r#"{"id":3,"do":"set_appear_offline","on":true}"#),
            Ok((3, Command::SetAppearOffline { on: true }))
        );
        assert_eq!(
            parse_command(
                r#"{"id":4,"do":"upload_trophies","title_id":"GAME00001",
                    "trophies":[{"number":3,"grade":"gold","earned":1800000000}]}"#
            ),
            Ok((
                4,
                Command::UploadTrophies {
                    title_id: "GAME00001".into(),
                    trophies: vec![anyplay_api::Trophy {
                        number: 3,
                        grade: anyplay_api::Grade::Gold,
                        earned: 1_800_000_000,
                    }],
                }
            ))
        );
        assert_eq!(
            parse_command(
                r#"{"id":6,"do":"run_report","title_id":"GAME00001","version":"01.000","toolkit":"0.2.0","verdict":"in_game"}"#
            ),
            Ok((
                6,
                Command::RunReport {
                    title_id: "GAME00001".into(),
                    version: "01.000".into(),
                    toolkit: "0.2.0".into(),
                    verdict: anyplay_api::Verdict::InGame,
                }
            ))
        );
        assert_eq!(
            parse_command(r#"{"id":7,"do":"run_reports","title_id":"GAME00001","version":null}"#),
            Ok((
                7,
                Command::RunReports {
                    title_id: "GAME00001".into(),
                    version: None,
                }
            ))
        );
        assert_eq!(
            parse_command(r#"{"id":5,"do":"set_privacy","privacy":{"trophies":"everyone"}}"#),
            Ok((
                5,
                Command::SetPrivacy {
                    privacy: anyplay_api::Privacy {
                        trophies: anyplay_api::Audience::Everyone,
                        ..Default::default()
                    },
                }
            ))
        );
        assert_eq!(
            parse_command(r#"{"id":6,"do":"privacy"}"#),
            Ok((6, Command::Privacy))
        );
        assert_eq!(
            parse_command(r#"{"id":7,"do":"public_profile"}"#),
            Ok((7, Command::PublicProfile))
        );
        assert_eq!(
            parse_command(r#"{"id":8,"do":"set_public_profile","on":true}"#),
            Ok((8, Command::SetPublicProfile { on: true }))
        );
        assert_eq!(
            parse_command(r#"{"id":3,"do":"feed","before":90,"limit":20}"#),
            Ok((
                3,
                Command::Feed {
                    before: Some(90),
                    limit: Some(20)
                }
            ))
        );
        assert_eq!(
            parse_command(r#"{"id":4,"do":"feed"}"#),
            Ok((
                4,
                Command::Feed {
                    before: None,
                    limit: None
                }
            ))
        );
        assert_eq!(
            parse_command(r#"{"id":5,"do":"react","item":77,"reaction":"gg"}"#),
            Ok((
                5,
                Command::React {
                    item: 77,
                    reaction: Some(anyplay_api::Reaction::Gg)
                }
            ))
        );
        assert_eq!(
            parse_command(r#"{"id":5,"do":"react","item":77,"reaction":null}"#),
            Ok((
                5,
                Command::React {
                    item: 77,
                    reaction: None
                }
            ))
        );
        assert_eq!(
            parse_command(r#"{"id":8,"do":"compare","account":42,"title_id":"GAME00001"}"#),
            Ok((
                8,
                Command::Compare {
                    account: AccountId(42),
                    title_id: "GAME00001".into()
                }
            ))
        );
        assert_eq!(
            parse_command(
                r#"{"id":8,"do":"secret_trophies","title_id":"GAME00001","numbers":[4,5]}"#
            ),
            Ok((
                8,
                Command::SecretTrophies {
                    title_id: "GAME00001".into(),
                    numbers: vec![4, 5]
                }
            ))
        );
        assert_eq!(
            parse_command(
                r#"{"id":8,"do":"reveal_trophies","title_id":"GAME00001","numbers":[4]}"#
            ),
            Ok((
                8,
                Command::RevealTrophies {
                    title_id: "GAME00001".into(),
                    numbers: vec![4]
                }
            ))
        );
        assert_eq!(
            parse_command(r#"{"id":9,"do":"profile","account":42}"#),
            Ok((
                9,
                Command::Profile {
                    account: AccountId(42)
                }
            ))
        );
        assert_eq!(
            parse_command(
                r#"{"id":12,"do":"upload_playtime",
                    "entries":[{"title_id":"GAME00001","day":"2026-10-24","minutes":95}]}"#
            ),
            Ok((
                12,
                Command::UploadPlaytime {
                    entries: vec![anyplay_api::PlaytimeEntry {
                        title_id: "GAME00001".into(),
                        day: "2026-10-24".into(),
                        minutes: 95,
                    }],
                }
            ))
        );
        assert_eq!(
            parse_command(r#"{"id":7,"do":"browser_sign_in","create":true}"#),
            Ok((7, Command::BrowserSignIn { create: true }))
        );
        assert_eq!(
            parse_command(r#"{"id":8,"do":"browser_sign_in"}"#),
            Ok((8, Command::BrowserSignIn { create: false }))
        );
        assert_eq!(
            parse_command(r#"{"id":9,"do":"account_page"}"#),
            Ok((9, Command::AccountPage))
        );
        assert_eq!(
            parse_command(r#"{"id":10,"do":"cancel_sign_in"}"#),
            Ok((10, Command::CancelSignIn))
        );
    }

    #[test]
    fn party_and_message_commands_are_lines() {
        assert_eq!(
            parse_command(r#"{"id":1,"do":"party_invite","accounts":[4,5]}"#),
            Ok((
                1,
                Command::PartyInvite {
                    accounts: vec![AccountId(4), AccountId(5)],
                    call: false
                }
            ))
        );
        assert_eq!(
            parse_command(r#"{"id":8,"do":"party_invite","accounts":[4],"call":true}"#),
            Ok((
                8,
                Command::PartyInvite {
                    accounts: vec![AccountId(4)],
                    call: true
                }
            ))
        );
        assert_eq!(
            parse_command(r#"{"id":4,"do":"party_create"}"#),
            Ok((4, Command::PartyCreate))
        );
        assert_eq!(
            parse_command(r#"{"id":9,"do":"calls_seen"}"#),
            Ok((9, Command::CallsSeen { account: None }))
        );
        assert_eq!(
            parse_command(r#"{"id":10,"do":"deafen","deafened":true}"#),
            Ok((10, Command::Deafen { deafened: true }))
        );
        assert_eq!(
            parse_command(r#"{"id":2,"do":"party_leave"}"#),
            Ok((2, Command::PartyLeave))
        );
        assert_eq!(
            parse_command(r#"{"id":3,"do":"messages","conversation":"c","after":7}"#),
            Ok((
                3,
                Command::Messages {
                    conversation: "c".into(),
                    after: Some(7),
                    before: None,
                    limit: None,
                    changed_after: None
                }
            ))
        );
        assert_eq!(
            parse_command(
                r#"{"id":4,"do":"react_message","conversation":"c","seq":3,"reaction":"gg"}"#
            ),
            Ok((
                4,
                Command::ReactMessage {
                    conversation: "c".into(),
                    seq: 3,
                    reaction: Some(anyplay_api::MessageReaction::Gg)
                }
            ))
        );
        assert_eq!(
            parse_command(
                r#"{"id":5,"do":"react_message","conversation":"c","seq":3,"reaction":null}"#
            ),
            Ok((
                5,
                Command::ReactMessage {
                    conversation: "c".into(),
                    seq: 3,
                    reaction: None
                }
            ))
        );
        assert_eq!(
            parse_command(
                r#"{"id":6,"do":"conversation_settings","conversation":"c","muted":true}"#
            ),
            Ok((
                6,
                Command::ConversationSettings {
                    conversation: "c".into(),
                    muted: Some(true),
                    pinned: None
                }
            ))
        );
        let send = Command::SendMessage {
            conversation: "c".into(),
            text: "hej \u{1F600}".into(),
            reply: Some(2),
        };
        assert_eq!(
            parse_command(&command_line(9, &send)),
            Ok((9, send.clone()))
        );
    }

    #[test]
    fn voice_commands_are_lines() {
        assert_eq!(
            parse_command(r#"{"id":1,"do":"voice","on":true}"#),
            Ok((1, Command::Voice { on: true }))
        );
        assert_eq!(
            parse_command(r#"{"id":7,"do":"voice_noise","enabled":false}"#),
            Ok((7, Command::VoiceNoise { enabled: false }))
        );
        assert_eq!(
            parse_command(r#"{"id":2,"do":"push_to_talk","talking":false}"#),
            Ok((2, Command::PushToTalk { talking: false }))
        );
        assert_eq!(
            parse_command(r#"{"id":3,"do":"voice_mode","mode":"voice_activity"}"#),
            Ok((
                3,
                Command::VoiceMode {
                    mode: VoiceMode::VoiceActivity
                }
            ))
        );
        assert_eq!(
            parse_command(r#"{"id":4,"do":"voice_mode","mode":"push_to_talk"}"#),
            Ok((
                4,
                Command::VoiceMode {
                    mode: VoiceMode::PushToTalk
                }
            ))
        );
        assert_eq!(
            parse_command(r#"{"id":5,"do":"peer_volume","account":7,"percent":150}"#),
            Ok((
                5,
                Command::PeerVolume {
                    account: AccountId(7),
                    percent: 150
                }
            ))
        );
        assert_eq!(
            parse_command(r#"{"id":6,"do":"peer_mute","account":7,"muted":true}"#),
            Ok((
                6,
                Command::PeerMute {
                    account: AccountId(7),
                    muted: true
                }
            ))
        );
        assert_eq!(
            parse_command(r#"{"id":7,"do":"voice_devices"}"#),
            Ok((7, Command::VoiceDevices))
        );
        assert_eq!(
            parse_command(r#"{"id":8,"do":"set_voice_device","kind":"input","name":"Headset"}"#),
            Ok((
                8,
                Command::SetVoiceDevice {
                    kind: DeviceKind::Input,
                    name: Some("Headset".into())
                }
            ))
        );
        assert_eq!(
            parse_command(r#"{"id":9,"do":"set_voice_device","kind":"output","name":null}"#),
            Ok((
                9,
                Command::SetVoiceDevice {
                    kind: DeviceKind::Output,
                    name: None
                }
            ))
        );
        // A volume above 65535 percent, or a mode that does not exist, is no command.
        assert_eq!(
            parse_command(r#"{"id":10,"do":"peer_volume","account":7,"percent":70000}"#),
            Err(Some(10))
        );
        assert_eq!(
            parse_command(r#"{"id":11,"do":"voice_mode","mode":"always"}"#),
            Err(Some(11))
        );
        let set = Command::SetVoiceDevice {
            kind: DeviceKind::Input,
            name: Some("Mic ä".into()),
        };
        assert_eq!(
            parse_command(&command_line(12, &set)),
            Ok((12, set.clone()))
        );
    }

    #[test]
    fn screen_commands_are_lines() {
        assert_eq!(
            parse_command(r#"{"id":1,"do":"screen_start"}"#),
            Ok((
                1,
                Command::ScreenStart {
                    source: ScreenSource::Game,
                    window: None,
                    pids: Vec::new(),
                }
            ))
        );
        assert_eq!(
            parse_command(r#"{"id":4,"do":"screen_start","window":65872,"pids":[100,101]}"#),
            Ok((
                4,
                Command::ScreenStart {
                    source: ScreenSource::Game,
                    window: Some(65872),
                    pids: vec![100, 101],
                }
            ))
        );
        assert_eq!(
            parse_command(r#"{"id":2,"do":"screen_start","source":"test_pattern"}"#),
            Ok((
                2,
                Command::ScreenStart {
                    source: ScreenSource::TestPattern,
                    window: None,
                    pids: Vec::new(),
                }
            ))
        );
        assert_eq!(
            parse_command(r#"{"id":3,"do":"screen_watch","account":9}"#),
            Ok((
                3,
                Command::ScreenWatch {
                    account: AccountId(9)
                }
            ))
        );
        assert_eq!(
            parse_command(r#"{"id":4,"do":"screen_stop"}"#),
            Ok((4, Command::ScreenStop))
        );
        assert_eq!(
            parse_command(r#"{"id":5,"do":"screen_leave"}"#),
            Ok((5, Command::ScreenLeave))
        );
        assert_eq!(
            parse_command(r#"{"id":6,"do":"screen_worker","path":"C:/w/anyplay-share.exe"}"#),
            Ok((
                6,
                Command::ScreenWorker {
                    path: "C:/w/anyplay-share.exe".into()
                }
            ))
        );
    }

    #[test]
    fn the_screen_event_is_one_line_and_not_taken_for_an_answer() {
        let event = Event::Screen {
            can_share: true,
            can_watch: true,
            hosting: Some(ScreenHosting {
                phase: ScreenPhase::Live,
                title: "Astro".into(),
                codec: Some("h264".into()),
                stats: ScreenStats {
                    width: 1280,
                    height: 720,
                    fps: 60,
                    kbps: 4000,
                },
                viewers: vec![ScreenViewer {
                    account: AccountId(7),
                    watching: true,
                    link: Some(Link::Direct),
                    rtt_ms: Some(24),
                    loss_percent: 0,
                }],
            }),
            offers: vec![ScreenOffer {
                account: AccountId(8),
                title: "Gran".into(),
                link: None,
            }],
            watching: None,
        };
        let output = Output::Event(event);
        let text = output_line(&output);
        assert!(text.starts_with("{\"event\":\"screen\",\"can_share\":true,"));
        assert!(text.contains("\"width\":1280,\"height\":720,\"fps\":60,\"kbps\":4000"));
        assert_eq!(text.matches('\n').count(), 1);
        assert_eq!(parse_output(&text), Some(output));
    }

    #[test]
    fn share_play_commands_parse_with_the_modes_the_spec_names() {
        use anyplay_api::ShareMode;
        assert_eq!(
            parse_command(r#"{"id":1,"do":"share_start","account":7,"mode":"hand_over"}"#),
            Ok((
                1,
                Command::ShareStart {
                    account: AccountId(7),
                    mode: ShareMode::HandOver
                }
            ))
        );
        assert_eq!(
            parse_command(r#"{"id":2,"do":"share_answer","invitation":"i1","accept":true}"#),
            Ok((
                2,
                Command::ShareAnswer {
                    id: "i1".into(),
                    accept: true
                }
            ))
        );
        assert_eq!(
            parse_command(r#"{"id":3,"do":"share_mode","mode":"together"}"#),
            Ok((
                3,
                Command::ShareMode {
                    mode: ShareMode::Together
                }
            ))
        );
        assert_eq!(
            parse_command(r#"{"id":4,"do":"share_request","mode":"watch"}"#),
            Ok((
                4,
                Command::ShareRequest {
                    mode: ShareMode::Watch
                }
            ))
        );
        assert_eq!(
            parse_command(r#"{"id":5,"do":"share_stop"}"#),
            Ok((5, Command::ShareStop))
        );
    }

    #[test]
    fn the_share_event_is_one_line_and_not_taken_for_an_answer() {
        let event = Event::Share {
            can_start: true,
            share: Some(ShareView {
                id: "s1".into(),
                role: ShareRole::Host,
                peer: Account {
                    id: AccountId(7),
                    handle: "ann".into(),
                },
                title: "Astro".into(),
                mode: anyplay_api::ShareMode::HandOver,
                phase: SharePhase::Live,
                link: Some(Link::Direct),
                codec: Some("h264".into()),
                stats: ShareStats {
                    width: 1280,
                    height: 720,
                    fps: 60,
                    kbps: 4000,
                    rtt_ms: Some(20),
                    loss_percent: 0,
                },
                requested: None,
            }),
        };
        let output = Output::Event(event);
        let text = output_line(&output);
        assert!(text.starts_with("{\"event\":\"share\",\"can_start\":true,"));
        assert!(text.contains("\"mode\":\"hand_over\""));
        assert_eq!(text.matches('\n').count(), 1);
        assert_eq!(parse_output(&text), Some(output));
        let none = Output::Event(Event::Share {
            can_start: false,
            share: None,
        });
        assert_eq!(parse_output(&output_line(&none)), Some(none));
    }

    #[test]
    fn the_voice_event_is_one_line_and_not_taken_for_an_answer() {
        let event = Event::Voice {
            state: VoiceState::On,
            enabled: true,
            mode: VoiceMode::PushToTalk,
            muted: true,
            deafened: false,
            me: VoiceLevel {
                level: 25,
                speaking: true,
            },
            peers: vec![
                VoicePeer {
                    account: AccountId(7),
                    level: VoiceLevel {
                        level: 0,
                        speaking: false,
                    },
                    link: Some(Link::Relay),
                },
                VoicePeer {
                    account: AccountId(8),
                    level: VoiceLevel::default(),
                    link: None,
                },
            ],
        };
        let output = Output::Event(event);
        let text = output_line(&output);
        assert_eq!(
            text,
            concat!(
                "{\"event\":\"voice\",\"state\":\"on\",\"enabled\":true,\"mode\":\"push_to_talk\",\"muted\":true,\"deafened\":false,",
                "\"me\":{\"level\":25,\"speaking\":true},\"peers\":[",
                "{\"account\":7,\"level\":0,\"speaking\":false,\"link\":\"relay\"},",
                "{\"account\":8,\"level\":0,\"speaking\":false,\"link\":null}]}\n"
            )
        );
        assert_eq!(parse_output(&text), Some(output));
        let devices = Answer::ok(
            3,
            VoiceDevices {
                input: vec!["Mic".into()],
                output: vec!["Speakers".into()],
                selected_input: None,
                selected_output: Some("Speakers".into()),
            },
        );
        assert_eq!(
            output_line(&Output::Answer(devices)),
            concat!(
                "{\"id\":3,\"ok\":{\"input\":[\"Mic\"],\"output\":[\"Speakers\"],",
                "\"selected_input\":null,\"selected_output\":\"Speakers\"}}\n"
            )
        );
    }

    #[test]
    fn message_events_are_not_taken_for_answers() {
        for event in [
            Event::Message {
                conversation: "c".into(),
                seq: 3,
            },
            Event::Typing {
                conversation: "c".into(),
                account: AccountId(4),
                typing: true,
            },
            Event::MessageUpdate {
                conversation: "c".into(),
                seq: 3,
                rev: 2,
            },
            Event::Delivered {
                conversation: "c".into(),
                account: AccountId(4),
                seq: 3,
            },
            Event::Conversation { id: "c".into() },
            Event::PartyInvitationGone { id: "i".into() },
            Event::Party { party: None },
            Event::Conversations(anyplay_api::Conversations::default()),
        ] {
            let output = Output::Event(event);
            let text = output_line(&output);
            assert_eq!(parse_output(&text), Some(output), "{text}");
        }
        assert_eq!(
            output_line(&Output::Event(Event::Message {
                conversation: "c".into(),
                seq: 3
            })),
            "{\"event\":\"message\",\"conversation\":\"c\",\"seq\":3}\n"
        );
    }

    #[test]
    fn the_browser_sign_in_answers_a_url_and_shows_as_pending() {
        let answer = output_line(&Output::Answer(Answer::ok(
            7,
            Opened {
                url: "https://cloud.example/web/authorize?x=1".into(),
            },
        )));
        assert_eq!(
            answer,
            "{\"id\":7,\"ok\":{\"url\":\"https://cloud.example/web/authorize?x=1\"}}\n"
        );
        let state = Output::Event(Event::State {
            cloud: Cloud::Offline,
            account: None,
            pending: Some(Pending::Browser),
        });
        let text = output_line(&state);
        assert!(text.contains("\"pending\":\"browser\""), "{text}");
        assert_eq!(parse_output(&text), Some(state));
    }

    #[test]
    fn an_unknown_command_keeps_its_number() {
        assert_eq!(parse_command(r#"{"id":3,"do":"teleport"}"#), Err(Some(3)));
        assert_eq!(parse_command(r#"{"do":"sign_out"}"#), Err(None));
        assert_eq!(parse_command("not json"), Err(None));
    }

    #[test]
    fn answers_and_events_tell_themselves_apart() {
        let ok = output_line(&Output::Answer(Answer::ok(4, EmailSent { code_length: 6 })));
        assert_eq!(ok, "{\"id\":4,\"ok\":{\"code_length\":6}}\n");
        let Some(Output::Answer(answer)) = parse_output(&ok) else {
            panic!("not an answer");
        };
        assert_eq!(
            answer.into_result::<EmailSent>(),
            Ok(EmailSent { code_length: 6 })
        );

        let error = Output::Answer(Answer::error(5, Failure::new("offline", "No cloud.")));
        let Some(Output::Answer(answer)) = parse_output(&output_line(&error)) else {
            panic!("not an answer");
        };
        assert_eq!(
            answer.into_result::<Done>(),
            Err(Failure::new("offline", "No cloud."))
        );

        let state = Output::Event(Event::State {
            cloud: Cloud::Online,
            account: None,
            pending: Some(Pending::Code),
        });
        let text = output_line(&state);
        assert_eq!(
            text,
            "{\"event\":\"state\",\"cloud\":\"online\",\"account\":null,\"pending\":\"code\"}\n"
        );
        assert_eq!(parse_output(&text), Some(state));
        assert_eq!(
            parse_output(r#"{"event":"meteor","size":3}"#),
            Some(Output::Event(Event::Unknown))
        );
    }

    fn api_error(
        code: ErrorCode,
        restriction: Option<anyplay_api::Restriction>,
    ) -> anyplay_api::ApiError {
        anyplay_api::ApiError {
            until: Some(2000),
            restriction,
            ..anyplay_api::ApiError::new(code, "No.")
        }
    }

    #[test]
    fn a_restricted_failure_says_what_and_until_when() {
        let failure = Failure::api(&api_error(
            ErrorCode::Restricted,
            Some(anyplay_api::Restriction::NewAccount),
        ));
        assert_eq!(failure.code, "restricted");
        assert_eq!(
            failure.restriction.as_deref(),
            Some(&Restriction::new(Restriction::NEW_ACCOUNT, Some(2000)))
        );
        let text = output_line(&Output::Answer(Answer::error(7, failure.clone())));
        assert!(
            text.contains(r#""restriction":{"kind":"new_account","until":2000}"#),
            "{text}"
        );
        let Some(Output::Answer(answer)) = parse_output(&text) else {
            panic!("not an answer");
        };
        assert_eq!(answer.into_result::<Done>(), Err(failure));

        let muted = Failure::api(&api_error(
            ErrorCode::Restricted,
            Some(anyplay_api::Restriction::Muted),
        ));
        assert_eq!(muted.restriction.unwrap().kind, "muted");
        // Other errors, and a `restricted` without its reason, carry none.
        assert_eq!(
            Failure::api(&api_error(ErrorCode::Limit, None)).restriction,
            None
        );
        assert_eq!(
            Failure::api(&api_error(ErrorCode::Restricted, None)).restriction,
            None
        );
    }

    #[test]
    fn a_failure_of_an_older_net_service_still_reads() {
        let text =
            r#"{"id":3,"error":{"code":"restricted","message":"A moderator muted this account."}}"#;
        let Some(Output::Answer(answer)) = parse_output(text) else {
            panic!("not an answer");
        };
        let failure = answer.into_result::<Done>().unwrap_err();
        assert!(failure.is(ErrorCode::Restricted) && failure.restriction.is_none());
        // And a restriction of a newer cloud, with a kind and a field this version lacks.
        let newer = r#"{"id":4,"error":{"code":"restricted","message":"x","restriction":{"kind":"quiet_hours","until":9,"colour":"red"}}}"#;
        let Some(Output::Answer(answer)) = parse_output(newer) else {
            panic!("not an answer");
        };
        let failure = answer.into_result::<Done>().unwrap_err();
        assert_eq!(failure.restriction.unwrap().kind, "quiet_hours");
    }

    #[test]
    fn a_ban_carries_its_statement_into_the_failure() {
        let statement = anyplay_api::Statement {
            id: 12,
            kind: anyplay_api::ActionKind::Ban,
            rule: anyplay_api::Rule::Cheating,
            reason: "Cheats in a game.".into(),
            created: 10,
            until: None,
            lifted: None,
            from_report: false,
            automated: false,
            appeal: anyplay_api::AppealInfo {
                status: anyplay_api::AppealStatus::Open,
                deadline: 500,
                text: Some("It was not me.".into()),
                answer: None,
                decided: None,
            },
        };
        let mut error = api_error(ErrorCode::Banned, None);
        error.until = None;
        error.ban = Some(Box::new(anyplay_api::BanNotice {
            statement: Some(statement),
            appeal_token: "secret".into(),
        }));
        let failure = Failure::api(&error);
        let restriction = failure.restriction.clone().map(|r| *r).unwrap();
        assert_eq!(restriction.kind, "banned");
        assert_eq!(restriction.until, None);
        assert_eq!(restriction.rule.as_deref(), Some("cheating"));
        assert_eq!(restriction.action, Some(12));
        assert_eq!(
            restriction.appeal,
            Some(Appeal {
                status: "open".into(),
                deadline: 500
            })
        );
        // The appeal token and the player's own text never leave the net service.
        let text = output_line(&Output::Answer(Answer::error(1, failure)));
        assert!(
            !text.contains("secret") && !text.contains("not me"),
            "{text}"
        );

        // A ban without a statement still says when it ends.
        let bare = Failure::api(&api_error(ErrorCode::Banned, None));
        assert_eq!(
            bare.restriction.as_deref(),
            Some(&Restriction::new(Restriction::BANNED, Some(2000)))
        );
    }

    #[test]
    fn client_too_old_is_a_failure_and_an_event() {
        let mut error = api_error(ErrorCode::ClientTooOld, None);
        error.until = None;
        error.minimum = Some("0.3.0".into());
        let failure = Failure::api(&error);
        assert_eq!(failure.code, "client_too_old");
        assert_eq!(failure.minimum.as_deref(), Some("0.3.0"));
        assert!(failure.is(ErrorCode::ClientTooOld));
        let text = output_line(&Output::Answer(Answer::error(4, failure.clone())));
        assert!(text.contains(r#""minimum":"0.3.0""#), "{text}");
        match parse_output(&text) {
            Some(Output::Answer(answer)) => {
                assert_eq!(answer.into_result::<()>().unwrap_err(), failure);
            }
            other => panic!("{other:?}"),
        }
        // A failure from an older service has no minimum, and others never print one.
        let older: Failure = serde_json::from_str(r#"{"code":"offline","message":"x"}"#).unwrap();
        assert_eq!(older.minimum, None);
        assert!(!output_line(&Output::Answer(Answer::error(1, older))).contains("minimum"));

        let event = Event::ClientTooOld {
            minimum: "0.3.0".into(),
        };
        let line = output_line(&Output::Event(event.clone()));
        assert_eq!(
            line.trim_end(),
            r#"{"event":"client_too_old","minimum":"0.3.0"}"#
        );
        assert_eq!(parse_output(&line), Some(Output::Event(event)));
    }

    #[test]
    fn a_warning_restricts_nothing_and_a_mute_does() {
        let mut statement = anyplay_api::Statement {
            id: 2,
            kind: anyplay_api::ActionKind::Warning,
            rule: anyplay_api::Rule::Spam,
            reason: "Stop.".into(),
            created: 1,
            until: Some(77),
            lifted: None,
            from_report: false,
            automated: false,
            appeal: anyplay_api::AppealInfo {
                status: anyplay_api::AppealStatus::Available,
                deadline: 9,
                text: None,
                answer: None,
                decided: None,
            },
        };
        assert_eq!(Restriction::of_statement(&statement), None);
        statement.kind = anyplay_api::ActionKind::Mute;
        let mute = Restriction::of_statement(&statement).unwrap();
        assert_eq!((mute.kind.as_str(), mute.until), ("muted", Some(77)));
        let event = Output::Event(Event::Restrictions {
            restrictions: vec![mute],
        });
        assert_eq!(parse_output(&output_line(&event)), Some(event));
    }

    #[test]
    fn the_appeal_command_reads() {
        assert_eq!(
            parse_command(r#"{"id":5,"do":"appeal","action":12,"text":"Please look again."}"#),
            Ok((
                5,
                Command::Appeal {
                    action: 12,
                    text: "Please look again.".into()
                }
            ))
        );
    }

    #[test]
    fn a_game_start_may_host_a_play_together_session() {
        let hosted = Command::GameStart {
            title_id: "GAME00001".into(),
            title: "Game".into(),
            version: "01.000".into(),
            build: "0.1.3".into(),
            join: None,
            version_rule: VersionRule::Exact,
            override_version: false,
            host: Some(Host {
                max_members: 8,
                visibility: Visibility::Friends,
                party: true,
            }),
        };
        let line = command_line(4, &hosted);
        assert!(
            line.contains("\"host\":{\"max_members\":8,\"visibility\":\"friends\",\"party\":true}")
        );
        assert_eq!(parse_command(&line), Ok((4, hosted)));
        // A launcher from before Play Together sends no `host`, and its line still reads.
        assert_eq!(
            parse_command(
                r#"{"id":5,"do":"game_start","title_id":"GAME00001","title":"Game","version":"01.000","build":"0.1.3","join":null}"#
            ),
            Ok((
                5,
                Command::GameStart {
                    title_id: "GAME00001".into(),
                    title: "Game".into(),
                    version: "01.000".into(),
                    build: "0.1.3".into(),
                    join: None,
                    host: None,
                    version_rule: VersionRule::Exact,
                    override_version: false,
                }
            ))
        );
        // The answer of an older net service has no session.
        let answer: GameStarted = serde_json::from_str(r#"{"env":{}}"#).unwrap();
        assert_eq!(answer.session, None);
    }

    #[test]
    fn a_join_may_carry_a_version_rule_and_an_override() {
        let check = Command::CheckJoin {
            session: "S1".into(),
            title_id: "GAME00001".into(),
            version: "01.050.000".into(),
            build: "0.1.3".into(),
            version_rule: VersionRule::Major,
            override_version: true,
        };
        let line = command_line(7, &check);
        assert!(line.contains("\"version_rule\":\"major\""), "{line}");
        assert!(line.contains("\"override_version\":true"), "{line}");
        assert_eq!(parse_command(&line), Ok((7, check)));
        // A launcher from before the rule sends neither, and its line reads as exact.
        let old = r#"{"id":8,"do":"check_join","session":"S1","title_id":"GAME00001","version":"1","build":"b"}"#;
        assert_eq!(
            parse_command(old),
            Ok((
                8,
                Command::CheckJoin {
                    session: "S1".into(),
                    title_id: "GAME00001".into(),
                    version: "1".into(),
                    build: "b".into(),
                    version_rule: VersionRule::Exact,
                    override_version: false,
                }
            ))
        );
        // The default stays off the line.
        let plain = Command::CheckJoin {
            session: "S1".into(),
            title_id: "GAME00001".into(),
            version: "1".into(),
            build: "b".into(),
            version_rule: VersionRule::Exact,
            override_version: false,
        };
        assert!(!command_line(9, &plain).contains("version_rule"));
    }

    // A command field named `id` once replaced the command's number on the line, so the net service
    // could not read the command and never answered it.
    #[test]
    fn answers_keep_the_command_number() {
        let commands = [
            Command::PartyAnswer {
                id: "p1".into(),
                accept: true,
            },
            Command::AnswerInvitation {
                id: "i1".into(),
                accept: false,
            },
            Command::Dialog {
                id: 3,
                accounts: None,
            },
        ];
        for command in commands {
            let line = command_line(7, &command);
            assert_eq!(line.matches("\"id\"").count(), 1, "{line}");
            assert_eq!(parse_command(&line), Ok((7, command)));
        }
        assert_eq!(
            parse_command(r#"{"id":9,"do":"party_answer","invitation":"p1","accept":true}"#),
            Ok((
                9,
                Command::PartyAnswer {
                    id: "p1".into(),
                    accept: true
                }
            ))
        );
    }

    #[test]
    fn friend_lists_travel_inside_the_event() {
        let event = Output::Event(Event::Friends(FriendLists::default()));
        let text = output_line(&event);
        assert!(text.starts_with("{\"event\":\"friends\",\"friends\":[]"));
        assert_eq!(parse_output(&text), Some(event));
    }

    // The launcher and the net service read each other's lines; a broken line from either is
    // refused, never a crash.

    #[test]
    fn damaged_lines_are_read_or_refused() {
        let lines = [
            command_line(
                7,
                &Command::Report {
                    account: AccountId(41),
                    reason: anyplay_api::ReportReason::Spam,
                    details: "äb".into(),
                    conversation: Some("c1".into()),
                    seq: Some(3),
                    listing: None,
                },
            ),
            command_line(
                8,
                &Command::Dialog {
                    id: 3,
                    accounts: Some(vec![AccountId(41), AccountId(42)]),
                },
            ),
            output_line(&Output::Event(Event::State {
                cloud: Cloud::Online,
                account: None,
                pending: Some(Pending::Code),
            })),
            output_line(&Output::Answer(Answer::error(
                5,
                Failure::new("offline", "No cloud."),
            ))),
            command_line(
                9,
                &Command::SendMessage {
                    conversation: "c1".into(),
                    text: "hej 😀".into(),
                    reply: None,
                },
            ),
            output_line(&Output::Event(Event::Message {
                conversation: "c1".into(),
                seq: 7,
            })),
            output_line(&Output::Event(Event::Read {
                conversation: "c1".into(),
                account: AccountId(4),
                seq: 7,
            })),
        ];
        // A fixed-seed xorshift, so every run is the same.
        let mut seed = 0x5EED_u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as usize
        };
        let (mut commands, mut outputs) = (0, 0);
        for _ in 0..20_000 {
            let mut bytes = lines[next() % lines.len()].as_bytes().to_vec();
            for _ in 0..1 + next() % 3 {
                let at = next() % bytes.len().max(1);
                match next() % 3 {
                    0 if !bytes.is_empty() => bytes[at] = next() as u8,
                    1 => bytes.truncate(at),
                    _ => bytes.insert(at.min(bytes.len()), next() as u8),
                }
            }
            let line = String::from_utf8_lossy(&bytes);
            if let Ok((id, command)) = parse_command(&line) {
                commands += 1;
                assert_eq!(
                    parse_command(&command_line(id, &command)),
                    Ok((id, command))
                );
            }
            if let Some(output) = parse_output(&line) {
                outputs += 1;
                assert_eq!(parse_output(&output_line(&output)), Some(output));
            }
        }
        assert!(
            commands > 100 && outputs > 100,
            "{commands} commands, {outputs} outputs"
        );
    }

    #[test]
    fn a_deeply_nested_line_is_refused() {
        let deep = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
        assert_eq!(parse_command(&deep), Err(None));
        assert_eq!(parse_output(&deep), None);
        let inside = format!(r#"{{"id":1,"do":"invite","accounts":{deep}}}"#);
        assert_eq!(parse_command(&inside), Err(None));
        let event = format!(r#"{{"event":"state","cloud":{deep}}}"#);
        assert_eq!(parse_output(&event), None);
    }
}
