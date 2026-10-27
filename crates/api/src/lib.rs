//! The cloud API: paths, request and answer bodies, events and errors.
//!
//! The API is specified in `spec/cloud-api.md`. The net service speaks it as a client and the cloud
//! as a server, so both build on these types and cannot drift apart.

use std::fmt;

use serde::{Deserialize, Serialize};

pub mod client;
pub mod moderation;
pub mod path;

pub use moderation::*;

/// The largest request body the cloud reads.
pub const MAX_BODY: usize = 64 * 1024;
/// The largest session properties, once decoded.
pub const MAX_PROPERTIES: usize = 8 * 1024;
/// The largest number of members a session can have.
pub const MAX_MEMBERS: u16 = 64;
/// The longest report details, in characters.
pub const MAX_REPORT_DETAILS: usize = 1000;
/// Trophy numbers are below this.
pub const MAX_TROPHY_NUMBER: u16 = 2048;
/// The most trophies one upload carries.
pub const MAX_TROPHY_UPLOAD: usize = 512;
/// The most accounts one request for gamer cards names.
pub const MAX_CARDS: usize = 100;
/// Rarity is given once this many players have a title, so it cannot reveal one player.
pub const RARITY_MIN_PLAYERS: u32 = 10;
/// A run summary is given once this many players reported, so it cannot point at one player.
pub const RUN_REPORTS_MIN: u32 = 3;
/// The longest game or toolkit version in a run report, in characters.
pub const MAX_RUN_VERSION: usize = 32;
/// The longest profile text, in characters.
pub const MAX_ABOUT: usize = 160;
/// The most favourite titles a profile lists.
pub const MAX_FAVOURITES: usize = 3;
/// The largest avatar picture an upload may carry, in bytes.
pub const MAX_AVATAR_UPLOAD: usize = 1024 * 1024;
/// Avatars are stored as squares of this many pixels on a side.
pub const AVATAR_SIZE: u32 = 256;
/// The most playtime entries one upload carries.
pub const MAX_PLAYTIME_UPLOAD: usize = 512;
/// The most items one feed page carries.
pub const MAX_FEED_PAGE: u32 = 50;
/// The items a feed page carries when the caller names no number.
pub const DEFAULT_FEED_PAGE: u32 = 30;
/// A day has this many minutes, the most one title can have on one day.
pub const MAX_DAY_MINUTES: u32 = 24 * 60;
/// Account IDs stay below 2^53 so every JSON parser reads them exactly.
pub const MAX_ACCOUNT_ID: u64 = (1 << 53) - 1;
/// The shortest and longest handle, in characters.
pub const HANDLE_LEN: std::ops::RangeInclusive<usize> = 3..=16;
/// The most members a party has.
pub const MAX_PARTY_MEMBERS: u16 = 8;
/// The most members a group conversation has.
pub const MAX_GROUP_MEMBERS: u16 = 32;
/// The longest message, in characters.
pub const MAX_MESSAGE_CHARS: usize = 2000;
/// The largest unread count the cloud reports for one conversation; more reads as this number,
/// and a client may show it as `100+`.
pub const MAX_UNREAD: u64 = 100;
/// The longest name of a group conversation, in characters.
pub const MAX_CONVERSATION_NAME: usize = 64;
/// The most messages one fetch answers with.
pub const MAX_MESSAGES_PAGE: u32 = 100;
/// How long after it was written the writer may edit a message, in seconds.
pub const EDIT_WINDOW_SECS: u64 = 15 * 60;
/// The longest text of a quoted message in a reply, in characters.
pub const MAX_QUOTE_CHARS: usize = 140;
/// The most conversations an account pins.
pub const MAX_PINNED: i64 = 5;
/// How long the text an unsend or an edit removed stays readable for a report, in seconds.
pub const REPORT_HOLD_SECS: u64 = 7 * 24 * 60 * 60;
/// How long a typing signal counts, in seconds. A client sends it again every
/// [`TYPING_RESEND_SECS`] while the player types and drops it after this long.
/// (The cloud itself accepts one per account and conversation every 2 seconds.)
pub const TYPING_TTL_SECS: u64 = 6;
/// The pace of a client's typing signals for one conversation.
pub const TYPING_RESEND_SECS: u64 = 3;

/// The number of an account. Games see it as the player's 64-bit account ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccountId(pub u64);

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Why a handle cannot be used, before the cloud's own list of reserved names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandleProblem {
    Length,
    FirstCharacter,
    Character,
}

/// True for a title ID that can stand in a path: 1 to 64 characters from `A-Z a-z 0-9 _ -`,
/// such as `GAME01234`.
pub fn is_title_id(text: &str) -> bool {
    (1..=64).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Checks the shape of a handle: 3 to 16 characters from `A-Z a-z 0-9 _ -`, a letter first.
pub fn check_handle(handle: &str) -> Result<(), HandleProblem> {
    if !HANDLE_LEN.contains(&handle.chars().count()) {
        return Err(HandleProblem::Length);
    }
    if !handle.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return Err(HandleProblem::FirstCharacter);
    }
    if !handle
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(HandleProblem::Character);
    }
    Ok(())
}

/// The look of a handle: what stays when the spellings that read the same in common UI fonts are
/// folded together. Two handles with the same skeleton pass for each other, so an account may
/// take a handle only when no other account has its skeleton.
///
/// The steps run in this order, and the cloud's migration backfills with the same ones in SQL:
///
/// 1. lower case (`A-Z` to `a-z`),
/// 2. drop `_` and `-`,
/// 3. `rn` to `m` and `vv` to `w` (two letters that read as one),
/// 4. `1` and `i` to `l` (small L, capital I and the digit one are the same stroke in many
///    sans-serif fonts; capital I is `i` after step 1),
/// 5. `0` to `o`.
///
/// Not mapped, on purpose: `5`/`s`, `2`/`z`, `8`/`b`, `u`/`v`, `cl`/`d`. They differ in most
/// fonts, and every mapping makes more honest handles unavailable. The skeleton is for comparing,
/// never for showing: the owner's spelling is kept.
pub fn handle_skeleton(handle: &str) -> String {
    handle
        .to_ascii_lowercase()
        .replace(['_', '-'], "")
        .replace("rn", "m")
        .replace("vv", "w")
        .replace(['1', 'i'], "l")
        .replace('0', "o")
}

/// Another account as anyone may see it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub id: AccountId,
    pub handle: String,
}

/// The signed-in account as only its owner sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Me {
    pub id: AccountId,
    pub handle: String,
    pub email: String,
    pub created: u64,
    /// The account appears offline to every other account (ADR 0033). False when a cloud does
    /// not send it.
    #[serde(default)]
    pub appear_offline: bool,
}

/// The setting that makes an account appear offline, alone: the body of
/// `PUT /v1/me/appear-offline` and its answer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppearOffline {
    pub appear_offline: bool,
}

/// Whether the player shows a page at `/u/<handle>` to people who are not their friends: the body of
/// `PUT /v1/me/public-profile` and its answer. Off for every new account.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicProfile {
    pub public_profile: bool,
}

/// How long two accounts must share a party or a session before the cloud remembers the pair
/// ("Played with recently").
pub const PLAYED_WITH_MIN_SECS: u64 = 120;
/// How long a pair is remembered after the last time, in days.
pub const PLAYED_WITH_DAYS: u64 = 30;
/// The most people one account remembers.
pub const PLAYED_WITH_MAX: usize = 50;

/// A person the account played with recently, in `GET /v1/me/played-with`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayedWith {
    pub account: Account,
    /// When they last shared a party or a session for long enough, in Unix seconds.
    pub last: u64,
    /// The title of that session. Absent after a party, or when the cloud does not know it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The two are friends already.
    #[serde(default)]
    pub friend: bool,
}

/// The answer of `GET /v1/me/played-with`: the people, newest first, and whether the cloud
/// is recording at all.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayedWithList {
    pub remember: bool,
    pub people: Vec<PlayedWith>,
}

/// Whether the cloud remembers the people the account plays with: the body of
/// `PUT /v1/me/played-with/remember` and its answer. On for every account (ADR 0038).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RememberPlayedWith {
    pub remember_played_with: bool,
}

impl Default for RememberPlayedWith {
    fn default() -> Self {
        Self {
            remember_played_with: true,
        }
    }
}

/// The tokens of one signed-in device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub access: String,
    pub access_expires: u64,
    pub refresh: String,
    pub device: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Windows,
    Linux,
    Macos,
    #[serde(other)]
    Other,
}

impl Platform {
    /// The platform this program was built for.
    pub fn current() -> Self {
        if cfg!(windows) {
            Platform::Windows
        } else if cfg!(target_os = "linux") {
            Platform::Linux
        } else if cfg!(target_os = "macos") {
            Platform::Macos
        } else {
            Platform::Other
        }
    }
}

/// A device as the player names it in the list of signed-in devices.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub name: String,
    pub platform: Platform,
    /// The device public key (see `anyplay-proof`), which binds the refresh token to it. A
    /// device without one is a legacy device: see "Device keys and proofs" in the spec.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

// Signing in

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmailRequest {
    pub email: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmailAnswer {
    pub challenge: String,
    pub expires: u64,
    pub code_length: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeRequest {
    pub challenge: String,
    pub code: String,
    pub device: Device,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum CodeAnswer {
    SignedIn { tokens: Tokens, me: Me },
    NewAccount { signup: String, expires: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignupRequest {
    pub signup: String,
    pub handle: String,
    pub age_confirmed: bool,
    pub device: Device,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedIn {
    pub tokens: Tokens,
    pub me: Me,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshRequest {
    pub refresh: String,
    /// Only for a legacy device: the public key to bind it to. The proof must be made with the
    /// matching private key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_key: Option<String>,
}

/// The second step of the browser sign-in: the one-time code the cloud sent to the loopback
/// address, the PKCE verifier, and the device that signs in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserTokenRequest {
    pub code: String,
    pub verifier: String,
    pub device: Device,
}

/// A one-time link that opens the account web, signed in as the caller account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountLink {
    pub url: String,
    pub expires: u64,
}

// Friends, blocks and presence

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Friend {
    pub account: Account,
    pub since: u64,
    pub presence: Presence,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub account: Account,
    pub since: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendLists {
    pub friends: Vec<Friend>,
    pub incoming: Vec<Request>,
    pub outgoing: Vec<Request>,
    pub blocked: Vec<Account>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendRequest {
    pub handle: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FriendState {
    Requested,
    Friends,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendRequestAnswer {
    pub state: FriendState,
}

/// What friends see about an account right now.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Presence {
    pub online: bool,
    pub activity: Option<Activity>,
    /// Rises whenever the cloud tells friends about a change. A client keeps the highest it has
    /// seen for each account and drops a copy that is not newer, however late it arrives. A
    /// cloud that does not count sends 0.
    #[serde(default)]
    pub revision: u64,
}

/// The game an account is playing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    pub title_id: String,
    pub title: String,
    pub version: String,
    /// Added by the cloud: the session friends may join, if any.
    #[serde(default)]
    pub session: Option<SessionRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRef {
    pub id: String,
    pub joinable: bool,
    pub members: u16,
    pub max_members: u16,
    /// What kind of session it is: a `lan` session reads "playing together" in presence.
    #[serde(default)]
    pub kind: SessionKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceRequest {
    pub activity: Option<Activity>,
}

// Sessions and invitations

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    Friends,
    Invite,
    Closed,
}

/// What a session is for ([ADR 0030](../../docs/adr/0030-play-together-is-a-session-that-ends-with-its-host.md)).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    /// Play Together: a session the launcher opens for a game that finds others on a local
    /// network. The leader's world is the game, so the session ends when the leader leaves.
    Lan,
    /// A session the game itself opens through its session calls. The default, what every
    /// session was before the field existed, and what a kind nobody knows is read as.
    #[default]
    #[serde(other)]
    Game,
}

impl SessionKind {
    /// The text of the kind in the API and in a cloud's store.
    pub fn as_str(self) -> &'static str {
        match self {
            SessionKind::Game => "game",
            SessionKind::Lan => "lan",
        }
    }

    /// The kind named by `text`; anything unknown is a `game` session.
    pub fn from_text(text: &str) -> Self {
        match text {
            "lan" => SessionKind::Lan,
            _ => SessionKind::Game,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title_id: String,
    pub title: String,
    pub version: String,
    pub build: String,
    /// Absent from a cloud that predates the field: a `game` session.
    #[serde(default)]
    pub kind: SessionKind,
    pub leader: AccountId,
    pub max_members: u16,
    pub visibility: Visibility,
    /// Opaque data for the game, as standard base64.
    pub properties: String,
    pub members: Vec<Member>,
    pub created: u64,
    /// Rises with every change to the session. A client drops a copy older than the one it has.
    /// A cloud that does not count sends 0.
    #[serde(default)]
    pub revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub account: Account,
    /// The member's tunnel key, an iroh endpoint ID in lowercase hex.
    pub endpoint: String,
    /// The relay the member's net service is reachable through, when it has one.
    #[serde(default)]
    pub relay: Option<String>,
    /// The member's virtual address in this session, such as `198.18.4.20`. It differs from the
    /// address of every other member of the session and stays the same for as long as the
    /// account is a member. Another session may give the same account another address, and
    /// another account the same one (ADR 0020).
    pub address: String,
    pub joined: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSession {
    pub title_id: String,
    pub title: String,
    pub version: String,
    pub build: String,
    #[serde(default, skip_serializing_if = "is_game")]
    pub kind: SessionKind,
    pub max_members: u16,
    pub visibility: Visibility,
    #[serde(default)]
    pub properties: String,
    pub endpoint: String,
    #[serde(default)]
    pub relay: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinSession {
    pub version: String,
    pub build: String,
    pub endpoint: String,
    #[serde(default)]
    pub relay: Option<String>,
    #[serde(default)]
    pub invitation: Option<String>,
    /// How far the joiner's game version may differ from the session's. The joiner's launcher
    /// reads it from its own table of games; the cloud knows no games and applies it as given.
    #[serde(default, skip_serializing_if = "is_exact")]
    pub version_rule: VersionRule,
    /// The player chose to join although the rule says the versions do not fit ("Try Anyway").
    /// Only the version check is skipped; the AnyPS5 build must still match.
    #[serde(default, skip_serializing_if = "is_false")]
    pub override_version: bool,
}

/// How far two versions of a game may differ for the players to play together. The version is the
/// game's own text, such as `01.044.000`, whose first dot-separated part is its major version.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionRule {
    /// The versions must be the same text. What every join was before the rule existed.
    #[default]
    Exact,
    /// The first part must be the same: `01.044.000` fits `01.050.000`, not `02.000.000`.
    Major,
    /// Any two versions fit.
    Any,
}

impl VersionRule {
    /// Do the host's and the joiner's versions fit under this rule?
    pub fn allows(self, host: &str, joiner: &str) -> bool {
        match self {
            VersionRule::Exact => host == joiner,
            VersionRule::Any => true,
            VersionRule::Major => {
                host == joiner || {
                    let major = |v: &str| v.split('.').next().unwrap_or_default().to_owned();
                    let (a, b) = (major(host), major(joiner));
                    !a.is_empty() && a == b
                }
            }
        }
    }
}

fn is_exact(rule: &VersionRule) -> bool {
    *rule == VersionRule::Exact
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateSession {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<Visibility>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_members: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leader: Option<AccountId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invite {
    pub to: Vec<AccountId>,
    /// A party invitation only: ring. A call invitation lives [`CALL_RING_SECONDS`] instead of
    /// 15 minutes and, unanswered, becomes a missed call.
    #[serde(default, skip_serializing_if = "is_false")]
    pub call: bool,
}

/// The longest Discord link of a party.
pub const MAX_DISCORD_LINK: usize = 200;

/// Whether `link` is a Discord address a party may name: `https://discord.com/channels/...` or
/// `https://discord.gg/...`, on those hosts only (no port, no credentials), without white space.
pub fn is_discord_link(link: &str) -> bool {
    if link.len() > MAX_DISCORD_LINK
        || link
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
    {
        return false;
    }
    let Some(rest) = link.strip_prefix("https://") else {
        return false;
    };
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let path = path.split(['?', '#']).next().unwrap_or("");
    match host {
        "discord.com" => path.starts_with("channels/") && path.len() > "channels/".len(),
        "discord.gg" => !path.is_empty(),
        _ => false,
    }
}

/// How long a call rings, in seconds.
pub const CALL_RING_SECONDS: u64 = 30;

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_game(kind: &SessionKind) -> bool {
    *kind == SessionKind::Game
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invited {
    pub sent: Vec<AccountId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invitation {
    pub id: String,
    pub from: Account,
    pub session: String,
    /// The kind of the session invited to. A `lan` invitation reads "Mira is playing Minecraft.
    /// Join?" and not "Mira invited you".
    #[serde(default)]
    pub kind: SessionKind,
    pub title_id: String,
    pub title: String,
    pub version: String,
    pub expires: u64,
}

// Discord Social SDK

/// What the SDK's authorisation gave the net service: the one-time code and the PKCE verifier
/// of the challenge it was asked with. The cloud trades them for tokens with its client secret.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscordCodeExchange {
    pub code: String,
    pub code_verifier: String,
    /// The redirect address the SDK reported; one of the two the cloud accepts.
    pub redirect_uri: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscordRefresh {
    pub refresh: String,
}

/// Discord's tokens for the player, in transit: the cloud keeps none of them.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscordTokens {
    pub access: String,
    pub refresh: String,
    /// Seconds from now until `access` ends.
    pub expires_in: u64,
    /// Space-separated scopes Discord granted.
    pub scope: String,
}

macro_rules! secret_debug {
    ($($name:ident { $($field:ident),* }),*) => {$(
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct(stringify!($name))$(.field(stringify!($field), &"<hidden>"))*.finish()
            }
        }
    )*};
}
secret_debug!(
    DiscordCodeExchange {
        code,
        code_verifier
    },
    DiscordRefresh { refresh },
    DiscordTokens { access, refresh }
);

// Relays, reports, information

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketRequest {
    pub endpoint: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Relay {
    pub url: String,
    pub token: String,
    pub expires: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Relays {
    pub relays: Vec<Relay>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportReason {
    Cheating,
    Harassment,
    Spam,
    Name,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub account: AccountId,
    pub reason: ReportReason,
    #[serde(default)]
    pub details: String,
    /// With `seq`: the message being reported, in a conversation the reporter can read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// The looking-for-group listing being reported; its note is saved with the report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listing: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Signup {
    Open,
    Closed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Info {
    pub name: String,
    pub version: String,
    pub signup: Signup,
    pub privacy: Option<String>,
    pub terms: Option<String>,
    /// The origin of the account web (sign in, create account, manage the account), absent when
    /// this cloud serves none. The browser sign-in starts at `<web>/web/authorize`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web: Option<String>,
    /// Whether this cloud offers share play, and the rate it allows on a relayed path. Absent
    /// from a cloud that predates share play; treat that as off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share: Option<ShareInfo>,
}

// Trophies and privacy

/// How valuable a trophy is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Grade {
    Bronze,
    Silver,
    Gold,
    Platinum,
}

/// A trophy a player earned: its number in the game, its grade and when. Its name and picture
/// come from the player's own copy of the game, never from the cloud.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trophy {
    pub number: u16,
    pub grade: Grade,
    /// Unix seconds.
    pub earned: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrophyUpload {
    pub trophies: Vec<Trophy>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrophiesAdded {
    pub added: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TitleTrophies {
    pub title_id: String,
    pub trophies: Vec<Trophy>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountTrophies {
    pub titles: Vec<TitleTrophies>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrophyRarity {
    pub number: u16,
    pub earned_by: u32,
}

/// How many of a title's players earned each trophy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rarity {
    pub title_id: String,
    /// Accounts with at least one record for the title; 0 below [`RARITY_MIN_PLAYERS`].
    pub players: u32,
    pub trophies: Vec<TrophyRarity>,
}

/// How a game ran for a player.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The game does not start.
    NoStart,
    /// It reaches the menus.
    Menus,
    /// It runs in a game.
    InGame,
    /// It is playable.
    Playable,
}

/// The system a run report was made on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Os {
    Windows,
    Linux,
    Macos,
}

/// What a player tells about one run: the body of `PUT /v1/me/run-reports/{title_id}`. The title is
/// in the path; nothing else about the player or the machine is part of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunReport {
    /// The version of the game.
    pub version: String,
    /// The version of the toolkit that ran it.
    pub toolkit: String,
    pub verdict: Verdict,
    pub os: Os,
}

/// True for a game or toolkit version of a run report: 1 to [`MAX_RUN_VERSION`] characters from
/// `A-Z a-z 0-9 . _ + -`, such as `01.000.002`.
pub fn is_run_version(text: &str) -> bool {
    (1..=MAX_RUN_VERSION).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-'))
}

/// How many reports each verdict has.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerdictCounts {
    pub no_start: u32,
    pub menus: u32,
    pub in_game: u32,
    pub playable: u32,
}

impl VerdictCounts {
    pub fn total(&self) -> u32 {
        self.no_start + self.menus + self.in_game + self.playable
    }

    pub fn add(&mut self, verdict: Verdict, n: u32) {
        let slot = match verdict {
            Verdict::NoStart => &mut self.no_start,
            Verdict::Menus => &mut self.menus,
            Verdict::InGame => &mut self.in_game,
            Verdict::Playable => &mut self.playable,
        };
        *slot += n;
    }

    /// The most reported verdict; on a tie the lower one, so that a summary promises no more than
    /// the players found. `None` without reports.
    pub fn top(&self) -> Option<Verdict> {
        [
            (Verdict::NoStart, self.no_start),
            (Verdict::Menus, self.menus),
            (Verdict::InGame, self.in_game),
            (Verdict::Playable, self.playable),
        ]
        .into_iter()
        .filter(|(_, n)| *n > 0)
        .fold(None, |best: Option<(Verdict, u32)>, (v, n)| match best {
            Some((_, m)) if m >= n => best,
            _ => Some((v, n)),
        })
        .map(|(v, _)| v)
    }
}

/// What players found, from run reports: the answer of `GET /v1/titles/{title_id}/runs`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSummary {
    pub title_id: String,
    /// The game version the counts are about, or null for all versions of the title.
    pub version: Option<String>,
    /// Reports counted; 0 below [`RUN_REPORTS_MIN`].
    pub reports: u32,
    /// The most reported verdict, or null below [`RUN_REPORTS_MIN`].
    pub verdict: Option<Verdict>,
    pub counts: VerdictCounts,
}

/// Trophies by grade.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Count {
    pub bronze: u32,
    pub silver: u32,
    pub gold: u32,
    pub platinum: u32,
}

impl Grade {
    /// The trophy points a trophy of this grade is worth (ADR 0029, `docs/adr`).
    pub const fn points(self) -> u32 {
        match self {
            Grade::Bronze => 10,
            Grade::Silver => 25,
            Grade::Gold => 60,
            Grade::Platinum => 150,
        }
    }
}

impl Count {
    /// The trophy points of these trophies; saturates instead of overflowing.
    pub fn points(&self) -> u32 {
        let sum = u64::from(self.bronze) * u64::from(Grade::Bronze.points())
            + u64::from(self.silver) * u64::from(Grade::Silver.points())
            + u64::from(self.gold) * u64::from(Grade::Gold.points())
            + u64::from(self.platinum) * u64::from(Grade::Platinum.points());
        u32::try_from(sum).unwrap_or(u32::MAX)
    }
}

/// The highest trophy level.
pub const MAX_TROPHY_LEVEL: u16 = 100;

/// The trophy points at which `level` starts: `6 * (level - 1) * (level + 10)`. Level 1 starts at
/// 0 and each step costs 12 points more than the one before. A level above
/// [`MAX_TROPHY_LEVEL`] counts as the highest.
pub const fn level_floor(level: u16) -> u32 {
    let level = if level > MAX_TROPHY_LEVEL {
        MAX_TROPHY_LEVEL
    } else {
        level
    } as u32;
    if level <= 1 {
        0
    } else {
        6 * (level - 1) * (level + 10)
    }
}

/// A player's trophy points and the level they make (ADR 0029, `docs/adr`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrophyLevel {
    /// All the points the player's trophies are worth.
    pub points: u32,
    /// 1 to [`MAX_TROPHY_LEVEL`].
    pub level: u16,
    /// The points at which this level started.
    pub from: u32,
    /// The points at which the next level starts; `None` at the highest level.
    pub to: Option<u32>,
}

impl TrophyLevel {
    /// The level that `points` make.
    pub fn of(points: u32) -> Self {
        let mut level = 1;
        while level < MAX_TROPHY_LEVEL && points >= level_floor(level + 1) {
            level += 1;
        }
        Self {
            points,
            level,
            from: level_floor(level),
            to: (level < MAX_TROPHY_LEVEL).then(|| level_floor(level + 1)),
        }
    }
}

/// Trophies counted by grade, with the points they are worth.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tally {
    pub counts: Count,
    pub points: u32,
}

impl Tally {
    pub fn of(counts: Count) -> Self {
        Self {
            counts,
            points: counts.points(),
        }
    }
}

/// One player's side of a trophy comparison.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompareSide {
    pub account: u64,
    /// The player's trophies in the title, by number: only what the caller may see.
    pub trophies: Vec<Trophy>,
    /// The trophies above, counted, with their points.
    pub title: Tally,
    /// All of the player's trophies the caller may see, in every title.
    pub total: Tally,
    /// The level the `total` makes.
    pub level: TrophyLevel,
}

/// The answer of `GET /v1/accounts/{id}/compare/{title}`: the caller (`you`) next to another
/// player (`them`) in one title.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrophyCompare {
    pub title_id: String,
    pub you: CompareSide,
    pub them: CompareSide,
}

/// Trophy numbers of one title, to mark as secret or to reveal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrophyNumbers {
    pub numbers: Vec<u16>,
}

/// How many trophies a mark or a reveal changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretsChanged {
    pub changed: u32,
}

/// The short form of a profile that friends see in their list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Card {
    pub account: Account,
    /// `None` when the account's trophies are hidden from the caller.
    pub trophies: Option<Count>,
    /// The hash of the avatar, `None` when there is none or it is hidden from the caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cards {
    pub cards: Vec<Card>,
}

/// Who may see a part of a profile.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Audience {
    /// Friends only, the default.
    #[default]
    Friends,
    Everyone,
    Nobody,
}

/// What a player shows to whom. A part left out of the JSON has the default, friends.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Privacy {
    pub trophies: Audience,
    pub about: Audience,
    pub favourites: Audience,
    pub avatar: Audience,
    pub playtime: Audience,
    /// Who sees the player's activity in their feeds. `nobody` also stops the cloud from
    /// recording new items.
    pub feed: Audience,
}

/// A part of a profile that has a visibility of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Part {
    Trophies,
    About,
    Favourites,
    Avatar,
    Playtime,
    Feed,
}

impl Privacy {
    /// Who may see `part`.
    pub fn audience(&self, part: Part) -> Audience {
        match part {
            Part::Trophies => self.trophies,
            Part::About => self.about,
            Part::Favourites => self.favourites,
            Part::Avatar => self.avatar,
            Part::Playtime => self.playtime,
            Part::Feed => self.feed,
        }
    }
}

// Profiles, avatars and playtime

/// Why a profile text cannot be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AboutProblem {
    TooLong,
    /// A control or direction-changing character.
    Character,
    Link,
}

/// Checks a profile text: at most [`MAX_ABOUT`] characters of plain text, no control characters
/// and no links.
pub fn check_about(text: &str) -> Result<(), AboutProblem> {
    if text.chars().count() > MAX_ABOUT {
        return Err(AboutProblem::TooLong);
    }
    check_about_chars(text)
}

/// The rules of a short plain text that do not depend on its length: no control or
/// direction-changing characters and no links.
fn check_about_chars(text: &str) -> Result<(), AboutProblem> {
    if text.chars().any(|c| {
        c.is_control()
            || matches!(
                c,
                '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}'
            )
    }) {
        return Err(AboutProblem::Character);
    }
    if contains_link(text) {
        return Err(AboutProblem::Link);
    }
    Ok(())
}

/// True for text that holds an address a reader could follow: a scheme with `:`, `www.`, or a
/// word of letters and digits joined by dots that ends in letters, such as `example.com`.
pub fn contains_link(text: &str) -> bool {
    let lower = text.to_lowercase();
    if lower.contains("://") || lower.contains("www.") {
        return true;
    }
    for scheme in ["mailto:", "javascript:", "data:", "file:", "tel:"] {
        if lower.contains(scheme) {
            return true;
        }
    }
    // A dot between a letter or digit and a run of 2 to 24 letters is a domain.
    let chars: Vec<char> = lower.chars().collect();
    chars.iter().enumerate().any(|(n, &c)| {
        if c != '.' || n == 0 || !chars[n - 1].is_alphanumeric() {
            return false;
        }
        let run = chars[n + 1..]
            .iter()
            .take_while(|c| c.is_alphabetic())
            .count();
        (2..=24).contains(&run) && !chars.get(n + 1 + run).is_some_and(|c| c.is_alphanumeric())
    })
}

/// Why a list of favourite titles cannot be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FavouritesProblem {
    TooMany,
    TitleId,
    Duplicate,
}

/// Checks a list of favourite titles: at most [`MAX_FAVOURITES`], each passing [`is_title_id`],
/// none twice.
pub fn check_favourites(titles: &[String]) -> Result<(), FavouritesProblem> {
    if titles.len() > MAX_FAVOURITES {
        return Err(FavouritesProblem::TooMany);
    }
    for (n, title) in titles.iter().enumerate() {
        if !is_title_id(title) {
            return Err(FavouritesProblem::TitleId);
        }
        if titles[..n].contains(title) {
            return Err(FavouritesProblem::Duplicate);
        }
    }
    Ok(())
}

/// A UTC date written `YYYY-MM-DD` as the number of days since 1970-01-01, or `None` when the text
/// is not a real date between the years 2000 and 2999.
pub fn day_number(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = text.get(range)?;
        if part.bytes().all(|b| b.is_ascii_digit()) {
            part.parse().ok()
        } else {
            None
        }
    };
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    if !(2000..=2999).contains(&year) || !(1..=12).contains(&month) || day < 1 {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let length = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if day > length {
        return None;
    }
    // Days from civil, after Howard Hinnant.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// What a profile shows. A part is `None` when it is not set or the caller may not see it; the two
/// look the same on purpose.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub account: Account,
    pub about: Option<String>,
    pub favourites: Option<Vec<String>>,
    /// The hash of the avatar picture; the picture is at `/v1/avatars/{hash}`.
    pub avatar: Option<String>,
    /// Total minutes per title.
    pub playtime: Option<Vec<TitlePlaytime>>,
}

/// New values for the parts of a profile. A part left out stays as it is; an empty `about` or
/// list clears it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favourites: Option<Vec<String>>,
}

// Profile page and feed

/// A connection (a linked Discord or Steam account) on a profile, as the viewer may see it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileLink {
    /// `discord` or `steam`; a client treats a name it does not know as plain text.
    pub provider: String,
    /// The name the provider shows, when it gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// A profile with what a profile page shows besides it: the trophy counts and the connections the
/// viewer may see. The answer of `GET /v1/accounts/{id}/profile`; a client that knows only
/// [`Profile`] reads the same JSON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfilePage {
    #[serde(flatten)]
    pub profile: Profile,
    /// The trophies by grade; `None` when they are hidden from the caller.
    #[serde(default)]
    pub trophies: Option<Count>,
    /// The points and level of those trophies; `None` when they are hidden from the caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<TrophyLevel>,
    /// Never null: an empty list does not say whether there are hidden connections.
    #[serde(default)]
    pub links: Vec<ProfileLink>,
}

/// One of the four reactions to a feed item. Nothing else can be sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reaction {
    Nice,
    Gg,
    Wow,
    Same,
}

impl Reaction {
    /// All four, in the order a client shows them.
    pub const ALL: [Reaction; 4] = [Reaction::Nice, Reaction::Gg, Reaction::Wow, Reaction::Same];

    /// The name on the wire and in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            Reaction::Nice => "nice",
            Reaction::Gg => "gg",
            Reaction::Wow => "wow",
            Reaction::Same => "same",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.as_str() == text)
    }
}

/// How many accounts chose each reaction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionCounts {
    pub nice: u32,
    pub gg: u32,
    pub wow: u32,
    pub same: u32,
}

impl ReactionCounts {
    pub fn of(&self, reaction: Reaction) -> u32 {
        match reaction {
            Reaction::Nice => self.nice,
            Reaction::Gg => self.gg,
            Reaction::Wow => self.wow,
            Reaction::Same => self.same,
        }
    }

    pub fn add(&mut self, reaction: Reaction) {
        let slot = match reaction {
            Reaction::Nice => &mut self.nice,
            Reaction::Gg => &mut self.gg,
            Reaction::Wow => &mut self.wow,
            Reaction::Same => &mut self.same,
        };
        *slot = slot.saturating_add(1);
    }
}

/// What happened. The cloud records no names or texts of games or trophies: a title and a trophy
/// are numbers, and a client shows their names from its own copy of the game.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FeedEvent {
    /// The account earned a trophy. `rarity` is the share of the title's players who earned it,
    /// in percent with one decimal, or null while too few players have the title
    /// ([`RARITY_MIN_PLAYERS`]).
    Trophy {
        title_id: String,
        number: u16,
        grade: Grade,
        #[serde(default)]
        rarity: Option<f32>,
    },
    /// The account and `with` became friends.
    Friend { with: Account },
    /// The account played the title for the first time.
    FirstPlayed { title_id: String },
    /// The account started a party. Only its members see this item.
    Party { members: Vec<Account> },
}

/// One entry in a feed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FeedItem {
    /// Rises with time; pass the smallest one held as `before` to read older items.
    pub id: u64,
    pub account: Account,
    /// Unix seconds.
    pub at: u64,
    #[serde(flatten)]
    pub event: FeedEvent,
    pub reactions: ReactionCounts,
    /// The caller's own reaction.
    #[serde(default)]
    pub mine: Option<Reaction>,
}

/// A page of a feed, newest first.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Feed {
    pub items: Vec<FeedItem>,
    /// Pass it as `before` for the next page; null at the end.
    #[serde(default)]
    pub next: Option<u64>,
}

/// The body of `PUT /v1/feed/{id}/reaction`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionRequest {
    pub reaction: Reaction,
}

/// An item's reactions after a change.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reactions {
    pub reactions: ReactionCounts,
    pub mine: Option<Reaction>,
}

/// The answer to an avatar upload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvatarSet {
    pub avatar: String,
}

/// True for the hash of an avatar: 64 lowercase hexadecimal digits.
pub fn is_avatar_hash(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Minutes played of one title on one UTC day.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaytimeEntry {
    pub title_id: String,
    /// `YYYY-MM-DD`, UTC.
    pub day: String,
    pub minutes: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaytimeUpload {
    pub entries: Vec<PlaytimeEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaytimeAdded {
    /// How many entries were new or raised the minutes kept for their day.
    pub changed: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TitlePlaytime {
    pub title_id: String,
    pub minutes: u64,
}

// Events

/// One message on the event connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Welcome {
        /// The address this account gets in a session whenever the session has not given it to
        /// another member: the same every time, from any session, and not leased. A client
        /// takes its address in a session from the session's member list; this one is for
        /// before it is in any, and for clients that predate session addresses (ADR 0020).
        #[serde(default)]
        address: String,
        server_time: u64,
    },
    Friends,
    Presence {
        account: AccountId,
        presence: Presence,
    },
    Invitation {
        invitation: Invitation,
    },
    InvitationGone {
        id: String,
    },
    Session {
        session: Session,
    },
    SessionGone {
        id: String,
    },
    SignedOut,
    /// The account was banned. The connection closes after this event.
    Banned {
        until: Option<u64>,
        /// Why, as the player reads it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        statement: Option<Box<Statement>>,
    },
    /// A moderator took an action that leaves the account signed in (a warning or a mute), or an
    /// appeal against an action was decided.
    Action {
        statement: Box<Statement>,
    },
    // Parties and messages
    /// The party this account is in changed. A client drops a copy whose revision is older.
    Party {
        party: Party,
    },
    /// This account is no longer in the party: it left, was removed or the party ended.
    PartyGone {
        id: String,
    },
    PartyInvitation {
        invitation: PartyInvitation,
    },
    PartyInvitationGone {
        id: String,
    },
    /// A friend called and the call ran out or was hung up before this account answered.
    MissedCall {
        call: MissedCall,
    },
    /// A missed call was marked seen, here or on another device of this account.
    MissedCallGone {
        account: AccountId,
    },
    /// A call this account placed ended without being answered.
    CallEnded {
        account: AccountId,
        reason: CallEnd,
    },
    /// A new message in a conversation, by its number. Fetch it with
    /// `GET /v1/conversations/{id}/messages?after=<last number known>`.
    Message {
        conversation: String,
        seq: u64,
    },
    /// Someone who is not a friend wrote for the first time. Fetch the requests.
    MessageRequest {
        conversation: String,
    },
    /// A member read a conversation up to `seq`.
    Read {
        conversation: String,
        account: AccountId,
        seq: u64,
    },
    /// A member's device received a direct conversation's messages up to `seq`.
    Delivered {
        conversation: String,
        account: AccountId,
        seq: u64,
    },
    /// A member is typing in the conversation, or stopped. Never stored; a client drops it
    /// after [`TYPING_TTL_SECS`].
    Typing {
        conversation: String,
        account: AccountId,
        typing: bool,
    },
    /// A message was edited, unsent or reacted to. Fetch the changes with
    /// `GET /v1/conversations/{id}/messages?changed_after=<last rev known>`.
    MessageUpdate {
        conversation: String,
        seq: u64,
        rev: u64,
    },
    /// The members or the name of a conversation changed, or there is a new one. Fetch it.
    Conversation {
        id: String,
    },
    /// This account is no longer in the conversation.
    ConversationGone {
        id: String,
    },
    /// A feed has news: a friend did something, or someone reacted to an item of this account.
    /// It carries nothing; fetch `GET /v1/feed`.
    Feed,
    /// The account's own appear-offline setting changed, on another device or in the portal.
    /// Sent to this account's devices only.
    AppearOffline {
        appear_offline: bool,
    },
    /// Someone asked to join a listing of this account. Fetch `GET /v1/lfg/mine`.
    LfgRequest {
        listing: String,
        title_id: String,
        from: Account,
    },
    /// The poster of a listing this account asked to join accepted. A party invitation from
    /// them is waiting.
    LfgAccepted {
        listing: String,
        title_id: String,
        by: Account,
    },
    // Share play
    /// A party member invited this account to share play.
    ShareInvitation {
        invitation: ShareInvitation,
    },
    /// A share invitation expired, was declined or its share ended.
    ShareInvitationGone {
        id: String,
    },
    /// The share this account is in changed. A client drops a copy whose revision is older.
    Share {
        share: Box<Share>,
    },
    /// This account is no longer in the share.
    ShareGone {
        id: String,
        reason: ShareGoneReason,
    },
    /// An event this version does not know.
    #[serde(other)]
    Unknown,
}

/// Close codes of the event connection.
pub mod close {
    /// The cloud's node is shutting down. Reconnect at a random moment within
    /// [`RECONNECT_SPREAD_SECS`].
    pub const GOING_AWAY: u16 = 1001;
    /// The cloud's node restarts; the same as [`GOING_AWAY`].
    pub const RESTARTING: u16 = 1012;
    pub const TOKEN_EXPIRED: u16 = 4001;
    pub const BANNED: u16 = 4003;
    pub const SIGNED_OUT: u16 = 4004;

    /// After [`GOING_AWAY`] or [`RESTARTING`], clients spread their reconnects over this many
    /// seconds, so that a node taken down for a deploy does not send them all back at once.
    pub const RECONNECT_SPREAD_SECS: u64 = 10;
}

// Errors

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    CodeWrong,
    CodeExpired,
    HandleInvalid,
    Unauthorized,
    Banned,
    Forbidden,
    /// The account may not do this yet or at the moment; the body adds `restriction`.
    Restricted,
    NotFound,
    HandleTaken,
    VersionMismatch,
    Full,
    Limit,
    TooLarge,
    RateLimited,
    /// Discord refused a code or a refresh token (`/me/discord/token`, `/me/discord/refresh`).
    GrantRefused,
    /// The client is older than the cloud allows; the body adds `minimum`.
    ClientTooOld,
    Internal,
    /// A code this version does not know.
    #[serde(other)]
    Unknown,
}

impl ErrorCode {
    /// The HTTP status the cloud answers with.
    pub fn status(self) -> u16 {
        match self {
            ErrorCode::BadRequest
            | ErrorCode::GrantRefused
            | ErrorCode::CodeWrong
            | ErrorCode::CodeExpired
            | ErrorCode::HandleInvalid => 400,
            ErrorCode::Unauthorized => 401,
            ErrorCode::Banned | ErrorCode::Forbidden | ErrorCode::Restricted => 403,
            ErrorCode::NotFound => 404,
            ErrorCode::HandleTaken
            | ErrorCode::VersionMismatch
            | ErrorCode::Full
            | ErrorCode::Limit => 409,
            ErrorCode::TooLarge => 413,
            ErrorCode::RateLimited => 429,
            ErrorCode::ClientTooOld => 426,
            ErrorCode::Internal | ErrorCode::Unknown => 500,
        }
    }

    /// The name on the wire, such as `not_found`.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::BadRequest => "bad_request",
            ErrorCode::CodeWrong => "code_wrong",
            ErrorCode::CodeExpired => "code_expired",
            ErrorCode::HandleInvalid => "handle_invalid",
            ErrorCode::Unauthorized => "unauthorized",
            ErrorCode::Banned => "banned",
            ErrorCode::Forbidden => "forbidden",
            ErrorCode::Restricted => "restricted",
            ErrorCode::NotFound => "not_found",
            ErrorCode::HandleTaken => "handle_taken",
            ErrorCode::VersionMismatch => "version_mismatch",
            ErrorCode::Full => "full",
            ErrorCode::Limit => "limit",
            ErrorCode::TooLarge => "too_large",
            ErrorCode::RateLimited => "rate_limited",
            ErrorCode::GrantRefused => "grant_refused",
            ErrorCode::ClientTooOld => "client_too_old",
            ErrorCode::Internal => "internal",
            ErrorCode::Unknown => "unknown",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The game version and AnyPS5 build a session expects.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Expected {
    pub version: String,
    pub build: String,
}

/// The body of every error answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    pub error: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<u64>,
    /// With `version_mismatch`: what the session expects. Boxed to keep `ApiError`, which
    /// travels in many `Result`s, small.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<Box<Expected>>,
    /// With `restricted`: why. `until` says when the restriction ends, if it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restriction: Option<Restriction>,
    /// With `banned`: why, and how to appeal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ban: Option<Box<BanNotice>>,
    /// With `client_too_old`: the oldest version of the client's product the cloud accepts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum: Option<Box<str>>,
}

impl ApiError {
    pub fn new(error: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            error,
            message: message.into(),
            until: None,
            expected: None,
            restriction: None,
            ban: None,
            minimum: None,
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.error, self.message)
    }
}

impl std::error::Error for ApiError {}

/// The first virtual address and the size of the range they come from (`198.18.0.0/15`).
pub const VIRTUAL_NETWORK: (std::net::Ipv4Addr, u8) = (std::net::Ipv4Addr::new(198, 18, 0, 0), 15);

/// True for an address inside the virtual range.
pub fn is_virtual(address: std::net::Ipv4Addr) -> bool {
    let (base, bits) = VIRTUAL_NETWORK;
    let mask = u32::MAX << (32 - bits);
    u32::from(address) & mask == u32::from(base)
}

/// How many addresses the virtual range holds.
pub const VIRTUAL_ADDRESSES: u32 = 1 << (32 - VIRTUAL_NETWORK.1 as u32);

/// How many addresses of the range a session can hand out: all but those ending in .0 or .255.
pub const ASSIGNABLE_ADDRESSES: u32 = VIRTUAL_ADDRESSES / 256 * 254;

// A session can always give every member an address of its own.
const _: () = assert!(MAX_MEMBERS as u32 <= ASSIGNABLE_ADDRESSES);

/// True for an address a session may hand out: inside the range, and not ending in .0 or .255,
/// which some games take for a network or broadcast address.
pub fn is_assignable(address: std::net::Ipv4Addr) -> bool {
    let last = address.octets()[3];
    is_virtual(address) && last != 0 && last != 255
}

/// The address at `slot` in the range, counted from its first address. `None` past the end.
pub fn virtual_address(slot: u32) -> Option<std::net::Ipv4Addr> {
    (slot < VIRTUAL_ADDRESSES)
        .then(|| std::net::Ipv4Addr::from(u32::from(VIRTUAL_NETWORK.0) + slot))
}

/// The place of an address in the range, counted from its first address.
pub fn virtual_slot(address: std::net::Ipv4Addr) -> Option<u32> {
    is_virtual(address).then(|| u32::from(address) - u32::from(VIRTUAL_NETWORK.0))
}

// Parties

/// A member of a party.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartyMember {
    pub account: Account,
    pub joined: u64,
    /// The tunnel key of the member's net service, an iroh endpoint ID in lowercase hex, once it
    /// has announced one (`PUT /v1/party/endpoint`). Empty before that. Only members of the party
    /// ever see it.
    #[serde(default)]
    pub endpoint: String,
    /// The relay that net service is reachable through, when it has one.
    #[serde(default)]
    pub relay: Option<String>,
    /// The member has party voice switched on: their net service accepts voice connections from
    /// the other members.
    #[serde(default)]
    pub voice: bool,
}

/// What a member's net service tells the party about itself, so the others can dial it for voice.
/// Sent again whenever one of the fields changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartyEndpoint {
    pub endpoint: String,
    #[serde(default)]
    pub relay: Option<String>,
    pub voice: bool,
}

/// A group of up to [`MAX_PARTY_MEMBERS`] friends that stays together across games. It belongs
/// to no game.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Party {
    pub id: String,
    pub leader: AccountId,
    /// Longest-standing first.
    pub members: Vec<PartyMember>,
    pub max_members: u16,
    /// The party's chat.
    pub conversation: String,
    pub created: u64,
    /// Rises with every change to the party. A client drops a copy older than the one it has.
    #[serde(default)]
    pub revision: u64,
    /// Where the party talks when its voice is on Discord: a `discord.com/channels` or
    /// `discord.gg` address the leader set. Every member sees it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discord: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartyInvitation {
    pub id: String,
    pub from: Account,
    pub party: String,
    /// How many members the party has now.
    pub members: u16,
    pub expires: u64,
    /// A call: it rings, and the party is the call.
    #[serde(default, skip_serializing_if = "is_false")]
    pub call: bool,
}

/// A call that was not answered, shown until the callee has seen it. One per caller: a newer
/// call replaces the older.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissedCall {
    pub from: Account,
    /// When the call began, Unix seconds.
    pub at: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissedCalls {
    pub missed: Vec<MissedCall>,
}

/// Why a call this account placed ended before it was answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallEnd {
    /// The callee declined.
    Declined,
    /// The call rang out.
    NoAnswer,
    /// A code this version does not know.
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartyInvitations {
    pub invitations: Vec<PartyInvitation>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateParty {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leader: Option<AccountId>,
    /// The Discord link of the party; the empty string removes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discord: Option<String>,
}

/// Moves the party into a session of its leader.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveParty {
    pub session: String,
}

// Share play

/// What a cloud says about share play in `GET /v1/info`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareInfo {
    /// False by default on a public cloud until the legal review is done; every share request
    /// is then answered `forbidden`.
    pub enabled: bool,
    /// The most a relayed share may stream, in kbit/s.
    pub relay_kbps: u32,
}

/// What the guest of a share may do with the host's pad.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareMode {
    Watch,
    HandOver,
    Together,
    /// A mode this version does not know.
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareState {
    /// The guest has been asked and has not accepted.
    Invited,
    Active,
    #[serde(other)]
    Unknown,
}

/// The path a share's connection takes, as its host's net service reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharePath {
    Direct,
    Relay,
    #[serde(other)]
    Unknown,
}

/// Why an account is no longer in a share.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareGoneReason {
    Ended,
    Declined,
    Left,
    GameEnded,
    Blocked,
    Banned,
    Timeout,
    Quota,
    #[serde(other)]
    Unknown,
}

/// A host streaming their game to one member of their party. The endpoints are given only to
/// the two parties; the invited guest sees `host_endpoint` only after accepting.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Share {
    pub id: String,
    pub host: Account,
    pub guest: Account,
    pub title_id: String,
    pub title: String,
    pub mode: ShareMode,
    pub state: ShareState,
    /// The tunnel key of the host's net service (an iroh endpoint ID in lowercase hex).
    #[serde(default)]
    pub host_endpoint: String,
    #[serde(default)]
    pub host_relay: Option<String>,
    /// Empty until the guest accepted.
    #[serde(default)]
    pub guest_endpoint: String,
    #[serde(default)]
    pub guest_relay: Option<String>,
    #[serde(default)]
    pub path: Option<SharePath>,
    pub created: u64,
    /// Rises with every change. A client drops a copy older than the one it has.
    #[serde(default)]
    pub revision: u64,
}

/// What the invited guest sees before accepting. It names the title and the mode.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareInvitation {
    pub id: String,
    pub from: Account,
    pub share: String,
    pub title_id: String,
    pub title: String,
    pub mode: ShareMode,
    pub expires: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareInvitations {
    pub invitations: Vec<ShareInvitation>,
}

/// `POST /v1/shares`: the host invites a party member.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateShare {
    pub to: AccountId,
    pub mode: ShareMode,
    /// The host's tunnel key and relay.
    pub endpoint: String,
    #[serde(default)]
    pub relay: Option<String>,
}

/// `POST /v1/share/accept`: the invited guest accepts with their tunnel key and relay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptShare {
    pub endpoint: String,
    #[serde(default)]
    pub relay: Option<String>,
}

/// `PATCH /v1/share`: the host changes what the share shows.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateShare {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<ShareMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<SharePath>,
}

// Conversations and messages

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationKind {
    Direct,
    Group,
    Party,
}

/// How far a member read a conversation, and how far their devices received it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadMark {
    pub account: AccountId,
    /// Read up to this number.
    pub seq: u64,
    /// A device received the messages up to this number (never below `seq`). 0 from a cloud
    /// that does not count.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub delivered: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// One of the six reactions to a message. Nothing else can be sent. A client draws them itself;
/// the wire carries the name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageReaction {
    Nice,
    Love,
    Laugh,
    Gg,
    Wow,
    Sad,
}

impl MessageReaction {
    /// All six, in the order a client shows them.
    pub const ALL: [MessageReaction; 6] = [
        MessageReaction::Nice,
        MessageReaction::Love,
        MessageReaction::Laugh,
        MessageReaction::Gg,
        MessageReaction::Wow,
        MessageReaction::Sad,
    ];

    /// The name on the wire and in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            MessageReaction::Nice => "nice",
            MessageReaction::Love => "love",
            MessageReaction::Laugh => "laugh",
            MessageReaction::Gg => "gg",
            MessageReaction::Wow => "wow",
            MessageReaction::Sad => "sad",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.as_str() == text)
    }
}

/// The accounts that chose one reaction on a message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionOn {
    pub reaction: MessageReaction,
    pub accounts: Vec<AccountId>,
}

/// The message a reply quotes, as the reader sees it now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quote {
    pub seq: u64,
    pub sender: Account,
    /// The first [`MAX_QUOTE_CHARS`] characters; empty when the quoted message was unsent.
    pub text: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub deleted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub kind: ConversationKind,
    /// The name of a group, if it has one.
    pub name: Option<String>,
    /// Everyone in it, the caller included.
    pub members: Vec<Account>,
    /// The number of the newest message, 0 for none. Numbers count up by one in each
    /// conversation, so a client that knows the number it has read up to knows what it misses.
    pub last: u64,
    /// The number the caller read up to.
    pub read: u64,
    /// How far the other members read. A member who has not accepted the conversation yet has
    /// no mark in it.
    pub marks: Vec<ReadMark>,
    /// Unix seconds of the newest message or of the conversation's start.
    pub updated: u64,
    /// True for a message request: the caller has not accepted it.
    pub request: bool,
    /// The party, for a party's chat.
    pub party: Option<String>,
    /// Messages of others above `read` that the caller can see: what an unread badge counts.
    /// The cloud counts, so every device shows the same number, up to [`MAX_UNREAD`].
    #[serde(default)]
    pub unread: u64,
    /// The caller muted the conversation: no toast, no badge, no notification.
    #[serde(default, skip_serializing_if = "is_false")]
    pub muted: bool,
    /// The caller pinned it: it comes first in the list.
    #[serde(default, skip_serializing_if = "is_false")]
    pub pinned: bool,
    /// Counts the changes to the conversation's messages (edits, unsends, reactions).
    #[serde(default)]
    pub rev: u64,
}

/// Changes the caller's own settings for a conversation. A field left out stays.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub muted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
}

/// Says that the caller types in a conversation, or stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypingSignal {
    pub typing: bool,
}

/// Changes the text of a message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditMessage {
    pub text: String,
}

/// Sets the caller's reaction to a message, replacing an earlier one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactToMessage {
    pub reaction: MessageReaction,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversations {
    /// Newest first.
    pub conversations: Vec<Conversation>,
    /// Message requests, newest first.
    pub requests: Vec<Conversation>,
}

/// Opens a conversation. One account opens (or finds) the conversation with that account; more
/// open a group.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateConversation {
    pub accounts: Vec<AccountId>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddToConversation {
    pub accounts: Vec<AccountId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Added {
    pub added: Vec<AccountId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendMessage {
    pub text: String,
    /// The number of a message in this conversation that this one answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub conversation: String,
    pub seq: u64,
    pub sender: Account,
    /// Empty for an unsent message.
    pub text: String,
    pub created: u64,
    /// The message this one answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<Quote>,
    /// Unix seconds of the last edit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited: Option<u64>,
    /// The writer unsent it: `text` is empty and nothing more can be done with it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub deleted: bool,
    /// Who chose which reaction, in the order of [`MessageReaction::ALL`]. Reactions of an
    /// account a block lies between are left out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reactions: Vec<ReactionOn>,
    /// The conversation's `rev` at this message's last change; 0 for one never changed.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub rev: u64,
    /// A screenshot or clip. The message has no text then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<MediaInfo>,
}

/// Messages in ascending order of `seq`, or of `rev` for `changed_after`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Messages {
    pub messages: Vec<Message>,
    /// The number of the conversation's newest message.
    pub last: u64,
    /// The conversation's `rev` now.
    #[serde(default)]
    pub rev: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkRead {
    pub seq: u64,
}

/// True for the characters that force a direction on the text around them: the embeddings and
/// overrides U+202A to U+202E and the isolates U+2066 to U+2069. A message that carries one can
/// make the text after it read in another order. The plain marks (U+200E, U+200F and U+061C) are
/// not in this set: they only settle which way punctuation faces, and Arabic and Hebrew writers
/// need them.
pub fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// True for message text the cloud takes: 1 to [`MAX_MESSAGE_CHARS`] characters, not only white
/// space, no control characters other than line breaks and tabs, and no direction overrides
/// (see [`is_bidi_control`]).
pub fn is_message_text(text: &str) -> bool {
    let chars = text.chars().count();
    (1..=MAX_MESSAGE_CHARS).contains(&chars)
        && !text.trim().is_empty()
        && !text
            .chars()
            .any(|c| (c.is_control() && !matches!(c, '\n' | '\t')) || is_bidi_control(c))
}

// Join codes

/// A code the cloud issued to the caller, and the address of the page it opens.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinCode {
    /// Ten characters; lower case letters and digits without `0`, `1`, `l` and `o`.
    pub code: String,
    /// The join page of the account web, ending in `/join/<code>`.
    pub url: String,
    pub expires: u64,
}

/// What a redeemed join code made: an ordinary invitation, which the caller still has to accept.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "invitation", rename_all = "lowercase")]
pub enum Redeemed {
    Session(Invitation),
    Party(PartyInvitation),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JoinKind {
    Session,
    Party,
}

/// The public face of a join code, for the page that opens it. Nothing else is revealed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinView {
    pub handle: String,
    /// The game's title as text; null for a party.
    pub title: Option<String>,
    pub members: u16,
    pub max_members: u16,
    pub kind: JoinKind,
}

/// The characters of a join code, and its length.
pub const JOIN_CODE_ALPHABET: &str = "23456789abcdefghijkmnpqrstuvwxyz";
pub const JOIN_CODE_LEN: usize = 10;

/// The code written in `text` (a code, a `anyps5://v1/join/<code>` link or a `…/join/<code>` web
/// address), in lower case; none when `text` holds no well-formed code.
pub fn join_code_from_text(text: &str) -> Option<String> {
    let text = text.trim();
    let tail = text.rsplit('/').find(|part| !part.is_empty())?;
    let tail = tail.split(['?', '#']).next()?.to_ascii_lowercase();
    (tail.len() == JOIN_CODE_LEN && tail.chars().all(|c| JOIN_CODE_ALPHABET.contains(c)))
        .then_some(tail)
}

// Looking for group

/// How long a listing stays open, in seconds.
pub const LFG_TTL_SECS: u64 = 60 * 60;

/// The longest note on a listing, in characters.
pub const MAX_LFG_NOTE: usize = 140;

/// The most places a listing can ask for: a party has [`MAX_PARTY_MEMBERS`], and the poster is one.
pub const MAX_LFG_SLOTS: u8 = (MAX_PARTY_MEMBERS - 1) as u8;

/// How many listings one account may keep open.
pub const MAX_LFG_OPEN: usize = 3;

/// How many open requests one listing holds. The next one is turned away with `full`.
pub const MAX_LFG_REQUESTS: usize = 20;

/// The most listings one answer carries.
pub const MAX_LFG_PAGE: usize = 50;

/// How the poster wants to play.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LfgMode {
    /// Online play through the network's platform services.
    Online,
    /// A virtual LAN.
    Lan,
    /// Play Together: one player's game, others join it.
    PlayTogether,
    /// A mode this version does not know.
    #[serde(other)]
    Unknown,
}

impl LfgMode {
    /// The name on the wire and in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            LfgMode::Online => "online",
            LfgMode::Lan => "lan",
            LfgMode::PlayTogether => "play_together",
            LfgMode::Unknown => "unknown",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        [LfgMode::Online, LfgMode::Lan, LfgMode::PlayTogether]
            .into_iter()
            .find(|m| m.as_str() == text)
    }
}

/// True for a language tag a listing can carry: two or three lower-case letters, such as `en`,
/// `de` or `tr`.
pub fn is_language_tag(text: &str) -> bool {
    (2..=3).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_lowercase())
}

/// Checks the note of a listing: at most [`MAX_LFG_NOTE`] characters of plain text, no control
/// characters and no links. The same rules as the profile text, with a shorter limit.
pub fn check_lfg_note(text: &str) -> Result<(), AboutProblem> {
    if text.chars().count() > MAX_LFG_NOTE {
        return Err(AboutProblem::TooLong);
    }
    check_about_chars(text)
}

/// A new listing. The poster has the game, which the cloud takes on trust.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateListing {
    pub title_id: String,
    pub mode: LfgMode,
    /// The language the poster plays in; none for any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Up to [`MAX_LFG_NOTE`] characters.
    #[serde(default)]
    pub note: String,
    /// How many players the poster looks for, 1 to [`MAX_LFG_SLOTS`].
    pub slots: u8,
}

/// Where the viewer's own request to a listing stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LfgAsk {
    Pending,
    /// The poster accepted: a party invitation is waiting.
    Accepted,
    #[serde(other)]
    Unknown,
}

/// A player who asked to join a listing, as its poster sees them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LfgRequest {
    pub from: Account,
    pub created: u64,
    pub accepted: bool,
}

/// A listing. Anyone who can see it sees everything but `requests`, which only the poster gets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    pub id: String,
    pub poster: Account,
    pub title_id: String,
    pub mode: LfgMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default)]
    pub note: String,
    /// How many players the poster looks for in all.
    pub slots: u8,
    /// How many of them the poster accepted already.
    pub filled: u8,
    pub created: u64,
    /// Unix seconds when the listing closes.
    pub expires: u64,
    /// The viewer's own request, if they asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask: Option<LfgAsk>,
    /// The poster's view only: who asked, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requests: Vec<LfgRequest>,
}

/// The open listings of one game, newest first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listings {
    pub listings: Vec<Listing>,
}

/// The caller's own listings, with their requests, and the listings the caller asked to join.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LfgMine {
    pub posted: Vec<Listing>,
    pub asked: Vec<Listing>,
}

// Media in messages

/// The largest screenshot, in bytes.
pub const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
/// The largest clip, in bytes.
pub const MAX_CLIP_BYTES: u64 = 100 * 1024 * 1024;
/// The longest clip, in seconds.
pub const MAX_CLIP_SECS: u32 = 60;
/// How long the cloud keeps a media file, in seconds.
pub const MEDIA_KEEP_SECS: u64 = 30 * 24 * 60 * 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    /// A PNG or JPEG screenshot.
    Image,
    /// An MP4 clip.
    Clip,
    #[serde(other)]
    Unknown,
}

impl MediaKind {
    /// The kind a `Content-Type` names: `image/png`, `image/jpeg` or `video/mp4`.
    pub fn from_content_type(content_type: &str) -> Option<Self> {
        match content_type.split(';').next()?.trim() {
            "image/png" | "image/jpeg" => Some(MediaKind::Image),
            "video/mp4" => Some(MediaKind::Clip),
            _ => None,
        }
    }

    /// The most bytes a file of this kind may have.
    pub fn max_bytes(self) -> u64 {
        match self {
            MediaKind::Clip => MAX_CLIP_BYTES,
            _ => MAX_IMAGE_BYTES,
        }
    }
}

/// The file a media message carries. The bytes come from `GET /v1/media/{id}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaInfo {
    pub id: String,
    pub kind: MediaKind,
    /// The `Content-Type` the file is served with.
    pub content_type: String,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// A clip's length in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u32>,
    /// Unix seconds when the cloud deletes the file.
    pub expires: u64,
}

#[cfg(test)]
mod join_code_tests {
    use super::*;

    #[test]
    fn the_code_is_read_out_of_a_link() {
        assert_eq!(
            join_code_from_text("k3x9m2pqra").as_deref(),
            Some("k3x9m2pqra")
        );
        assert_eq!(
            join_code_from_text("anyps5://v1/join/K3X9M2PQRA/").as_deref(),
            Some("k3x9m2pqra")
        );
        assert_eq!(
            join_code_from_text("https://anyplaynet.com/join/k3x9m2pqra?x=1").as_deref(),
            Some("k3x9m2pqra")
        );
        assert_eq!(join_code_from_text("k3x9m2pqr0"), None);
        assert_eq!(join_code_from_text("short"), None);
        assert_eq!(join_code_from_text(""), None);
    }

    fn person() -> Account {
        Account {
            id: AccountId(2),
            handle: "lumen".into(),
        }
    }

    #[test]
    fn a_session_without_a_kind_is_a_game_session() {
        // A cloud from before Play Together sends no kind.
        let old = r#"{"id":"S1","title_id":"GAME00001","title":"G","version":"1","build":"1","leader":1,
            "max_members":4,"visibility":"friends","properties":"","members":[],"created":0}"#;
        let session: Session = serde_json::from_str(old).unwrap();
        assert_eq!(session.kind, SessionKind::Game);
        let reference: SessionRef =
            serde_json::from_str(r#"{"id":"S1","joinable":true,"members":1,"max_members":4}"#)
                .unwrap();
        assert_eq!(reference.kind, SessionKind::Game);
        // A net service from before it asks for a game session by saying nothing.
        let create = CreateSession {
            title_id: "GAME00001".into(),
            title: "G".into(),
            version: "1".into(),
            build: "1".into(),
            kind: SessionKind::Game,
            max_members: 4,
            visibility: Visibility::Friends,
            properties: String::new(),
            endpoint: "ab".repeat(32),
            relay: None,
        };
        assert!(!serde_json::to_string(&create).unwrap().contains("kind"));
        let lan = CreateSession {
            kind: SessionKind::Lan,
            ..create
        };
        assert!(
            serde_json::to_string(&lan)
                .unwrap()
                .contains(r#""kind":"lan""#)
        );
        assert_eq!(SessionKind::from_text("lan"), SessionKind::Lan);
        assert_eq!(SessionKind::from_text("arena"), SessionKind::Game);
    }

    #[test]
    fn a_redeemed_code_is_its_kind_and_its_invitation() {
        let party = Redeemed::Party(PartyInvitation {
            id: "I1".into(),
            from: person(),
            party: "P1".into(),
            members: 2,
            expires: 5,
            call: false,
        });
        let json = serde_json::to_value(&party).unwrap();
        assert_eq!(json["kind"], "party");
        assert_eq!(json["invitation"]["party"], "P1");
        assert_eq!(serde_json::from_value::<Redeemed>(json).unwrap(), party);

        let session = Redeemed::Session(Invitation {
            id: "I2".into(),
            from: person(),
            session: "S1".into(),
            kind: SessionKind::Game,
            title_id: "GAME00001".into(),
            title: "Game".into(),
            version: "01.00".into(),
            expires: 5,
        });
        let json = serde_json::to_value(&session).unwrap();
        assert_eq!(json["kind"], "session");
        assert_eq!(serde_json::from_value::<Redeemed>(json).unwrap(), session);
    }

    #[test]
    fn a_version_rule_says_which_versions_fit() {
        use VersionRule::{Any, Exact, Major};
        assert!(Exact.allows("01.044.000", "01.044.000"));
        assert!(!Exact.allows("01.044.000", "01.050.000"));
        assert!(Major.allows("01.044.000", "01.050.000"));
        assert!(!Major.allows("01.044.000", "02.000.000"));
        assert!(Major.allows("1.2", "1"));
        assert!(Major.allows("weird", "weird"));
        assert!(!Major.allows(".1", ".2"));
        assert!(Any.allows("01.044.000", "02.000.000"));
        assert_eq!(VersionRule::default(), Exact);
    }

    #[test]
    fn a_join_without_a_rule_is_exact_and_the_default_stays_off_the_wire() {
        let old = r#"{"version":"1","build":"b","endpoint":"e"}"#;
        let join: JoinSession = serde_json::from_str(old).unwrap();
        assert_eq!(join.version_rule, VersionRule::Exact);
        assert!(!join.override_version);
        assert_eq!(
            serde_json::to_string(&join).unwrap(),
            r#"{"version":"1","build":"b","endpoint":"e","relay":null,"invitation":null}"#
        );
        let asked = JoinSession {
            version_rule: VersionRule::Major,
            override_version: true,
            ..join
        };
        let json = serde_json::to_value(&asked).unwrap();
        assert_eq!(json["version_rule"], "major");
        assert_eq!(json["override_version"], true);
    }

    #[test]
    fn the_join_types_keep_their_wire_shape() {
        let code = JoinCode {
            code: "k3x9m2pqra".into(),
            url: "https://anyplaynet.com/join/k3x9m2pqra".into(),
            expires: 9,
        };
        let json = serde_json::to_string(&code).unwrap();
        assert_eq!(
            json,
            r#"{"code":"k3x9m2pqra","url":"https://anyplaynet.com/join/k3x9m2pqra","expires":9}"#
        );
        assert_eq!(serde_json::from_str::<JoinCode>(&json).unwrap(), code);

        let view = JoinView {
            handle: "lumen".into(),
            title: None,
            members: 1,
            max_members: 8,
            kind: JoinKind::Party,
        };
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["kind"], "party");
        assert!(json["title"].is_null());
        assert_eq!(serde_json::from_value::<JoinView>(json).unwrap(), view);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discord_secrets_stay_out_of_debug_output() {
        let tokens = DiscordTokens {
            access: "access-secret".into(),
            refresh: "refresh-secret".into(),
            expires_in: 604_800,
            scope: "identify".into(),
        };
        let exchange = DiscordCodeExchange {
            code: "code-secret".into(),
            code_verifier: "verifier-secret".into(),
            redirect_uri: "http://127.0.0.1/callback".into(),
        };
        let text = format!(
            "{tokens:?} {exchange:?} {:?}",
            DiscordRefresh {
                refresh: "r-secret".into()
            }
        );
        assert!(!text.contains("secret"), "{text}");
    }

    #[test]
    fn a_refused_grant_is_a_bad_request_by_status() {
        assert_eq!(ErrorCode::GrantRefused.status(), 400);
        let body = ApiError::new(ErrorCode::GrantRefused, "Discord refused it.");
        let text = serde_json::to_string(&body).unwrap();
        assert!(text.contains(r#""error":"grant_refused""#), "{text}");
        assert_eq!(
            serde_json::from_str::<ApiError>(&text).unwrap().error,
            ErrorCode::GrantRefused
        );
    }

    #[test]
    fn message_text_has_bounds() {
        assert!(is_message_text("hi"));
        assert!(is_message_text("two\nlines\tand a tab"));
        assert!(is_message_text(&"ä".repeat(MAX_MESSAGE_CHARS)));
        assert!(!is_message_text(&"ä".repeat(MAX_MESSAGE_CHARS + 1)));
        assert!(!is_message_text(""));
        assert!(!is_message_text(" \n\t "));
        assert!(!is_message_text("bell\u{7}"));
    }

    #[test]
    fn message_text_refuses_direction_overrides() {
        for c in ('\u{202A}'..='\u{202E}').chain('\u{2066}'..='\u{2069}') {
            assert!(!is_message_text(&format!("a{c}b")), "U+{:04X}", c as u32);
            assert!(!is_message_text(&format!("{c}")));
        }
        // The plain marks stay: right-to-left writers need them.
        assert!(is_message_text("shalom\u{200F}!"));
        assert!(is_message_text("a\u{200E}b\u{61C}c"));
        // The neighbours of the ranges are ordinary characters.
        assert!(is_message_text("x\u{2029}\u{202F}\u{2065}\u{206A}"));
    }

    #[test]
    fn message_events_are_pointers() {
        let event = Event::Message {
            conversation: "c1".into(),
            seq: 7,
        };
        let text = serde_json::to_string(&event).unwrap();
        assert_eq!(text, r#"{"type":"message","conversation":"c1","seq":7}"#);
        assert_eq!(serde_json::from_str::<Event>(&text).unwrap(), event);
        assert_eq!(
            serde_json::from_str::<Event>(
                r#"{"type":"read","conversation":"c1","account":4,"seq":2}"#
            )
            .unwrap(),
            Event::Read {
                conversation: "c1".into(),
                account: AccountId(4),
                seq: 2
            }
        );
        assert_eq!(
            serde_json::from_str::<Event>(r#"{"type":"party_gone","id":"p"}"#).unwrap(),
            Event::PartyGone { id: "p".into() }
        );
    }

    #[test]
    fn share_play_travels_as_the_spec_names_it() {
        let share = Share {
            id: "s1".into(),
            host: Account {
                id: AccountId(2),
                handle: "lumen".into(),
            },
            guest: Account {
                id: AccountId(3),
                handle: "ann".into(),
            },
            title_id: "T".into(),
            title: "A game".into(),
            mode: ShareMode::HandOver,
            state: ShareState::Invited,
            host_endpoint: "ab".repeat(32),
            host_relay: None,
            guest_endpoint: String::new(),
            guest_relay: None,
            path: None,
            created: 5,
            revision: 2,
        };
        let json = serde_json::to_string(&Event::Share {
            share: Box::new(share.clone()),
        })
        .unwrap();
        assert!(
            json.starts_with(r#"{"type":"share","share":{"id":"s1""#),
            "{json}"
        );
        assert!(json.contains(r#""mode":"hand_over""#) && json.contains(r#""state":"invited""#));
        assert_eq!(
            serde_json::from_str::<Event>(&json).unwrap(),
            Event::Share {
                share: Box::new(share)
            }
        );
        assert_eq!(
            serde_json::from_str::<Event>(
                r#"{"type":"share_gone","id":"s1","reason":"game_ended"}"#
            )
            .unwrap(),
            Event::ShareGone {
                id: "s1".into(),
                reason: ShareGoneReason::GameEnded
            }
        );
        // A mode or reason of a newer cloud is read, not refused.
        assert_eq!(
            serde_json::from_str::<ShareMode>(r#""spectate""#).unwrap(),
            ShareMode::Unknown
        );
        assert_eq!(
            serde_json::from_str::<Event>(r#"{"type":"share_invitation_gone","id":"i"}"#).unwrap(),
            Event::ShareInvitationGone { id: "i".into() }
        );
        let body: CreateShare = serde_json::from_str(&format!(
            r#"{{"to":4,"mode":"watch","endpoint":"{}"}}"#,
            "ab".repeat(32)
        ))
        .unwrap();
        assert_eq!(
            (body.to, body.mode, body.relay),
            (AccountId(4), ShareMode::Watch, None)
        );
    }

    #[test]
    fn a_message_of_an_older_cloud_reads_and_a_plain_one_stays_small() {
        let old: Message = serde_json::from_str(
            r#"{"conversation":"c","seq":1,"sender":{"id":4,"handle":"Ann"},"text":"hi","created":9}"#,
        )
        .unwrap();
        assert_eq!(old.reply, None);
        assert!(!old.deleted && old.reactions.is_empty() && old.rev == 0);
        assert_eq!(
            serde_json::to_string(&old).unwrap(),
            r#"{"conversation":"c","seq":1,"sender":{"id":4,"handle":"Ann"},"text":"hi","created":9}"#
        );
    }

    #[test]
    fn reactions_have_names_and_nothing_else_parses() {
        for reaction in MessageReaction::ALL {
            assert_eq!(MessageReaction::parse(reaction.as_str()), Some(reaction));
            assert_eq!(
                serde_json::to_string(&reaction).unwrap(),
                format!("\"{}\"", reaction.as_str())
            );
        }
        assert_eq!(MessageReaction::parse("thumbs_up"), None);
        assert!(serde_json::from_str::<ReactToMessage>(r#"{"reaction":"\ud83d\udc4d"}"#).is_err());
    }

    #[test]
    fn typing_and_update_events_are_pointers() {
        let typing = Event::Typing {
            conversation: "c".into(),
            account: AccountId(4),
            typing: true,
        };
        assert_eq!(
            serde_json::to_string(&typing).unwrap(),
            r#"{"type":"typing","conversation":"c","account":4,"typing":true}"#
        );
        let update = Event::MessageUpdate {
            conversation: "c".into(),
            seq: 3,
            rev: 2,
        };
        assert_eq!(
            serde_json::to_string(&update).unwrap(),
            r#"{"type":"message_update","conversation":"c","seq":3,"rev":2}"#
        );
    }
    #[test]
    fn a_party_without_a_revision_reads_as_zero() {
        let party: Party = serde_json::from_str(
            r#"{"id":"p","leader":1,"members":[{"account":{"id":1,"handle":"Alec"},"joined":3}],
                "max_members":8,"conversation":"c","created":3}"#,
        )
        .unwrap();
        assert_eq!(party.revision, 0);
        assert_eq!(party.members.len(), 1);
    }

    #[test]
    fn title_ids_that_can_stand_in_a_path() {
        assert!(is_title_id("GAME01234"));
        assert!(is_title_id("GAME_00001-x"));
        assert!(!is_title_id(""));
        assert!(!is_title_id("../admin"));
        assert!(!is_title_id("GAME 01234"));
        assert!(!is_title_id("GAME01234?x=1"));
        assert!(!is_title_id(&"A".repeat(65)));
    }

    #[test]
    fn run_versions_and_the_top_verdict() {
        assert!(is_run_version("01.000.002"));
        assert!(is_run_version("0.2.0-rc+1"));
        assert!(!is_run_version(""));
        assert!(!is_run_version("a b"));
        assert!(!is_run_version(&"1".repeat(33)));
        let mut counts = VerdictCounts::default();
        assert_eq!(counts.top(), None);
        counts.add(Verdict::Playable, 2);
        counts.add(Verdict::Menus, 2);
        counts.add(Verdict::NoStart, 1);
        assert_eq!(counts.total(), 5);
        assert_eq!(counts.top(), Some(Verdict::Menus));
        counts.add(Verdict::Playable, 1);
        assert_eq!(counts.top(), Some(Verdict::Playable));
        let report: RunReport = serde_json::from_str(
            r#"{"version":"1.0","toolkit":"0.2","verdict":"no_start","os":"macos"}"#,
        )
        .unwrap();
        assert_eq!((report.verdict, report.os), (Verdict::NoStart, Os::Macos));
    }

    #[test]
    fn a_feed_item_is_flat_json_with_a_kind() {
        let item = FeedItem {
            id: 7,
            account: Account {
                id: AccountId(2),
                handle: "Lumen".into(),
            },
            at: 1_800_000_000,
            event: FeedEvent::Trophy {
                title_id: "GAME00001".into(),
                number: 4,
                grade: Grade::Gold,
                rarity: Some(2.5),
            },
            reactions: ReactionCounts {
                gg: 3,
                ..Default::default()
            },
            mine: Some(Reaction::Gg),
        };
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["kind"], "trophy");
        assert_eq!(json["grade"], "gold");
        assert_eq!(json["mine"], "gg");
        assert_eq!(json["reactions"]["gg"], 3);
        assert_eq!(serde_json::from_value::<FeedItem>(json).unwrap(), item);
        let friend: FeedItem = serde_json::from_str(
            r#"{"id":8,"account":{"id":2,"handle":"Lumen"},"at":1,"kind":"friend",
                "with":{"id":3,"handle":"Nox"},"reactions":{"nice":0,"gg":0,"wow":0,"same":0}}"#,
        )
        .unwrap();
        assert_eq!(friend.mine, None);
        assert!(matches!(friend.event, FeedEvent::Friend { .. }));
    }

    #[test]
    fn only_four_reactions_exist() {
        for reaction in Reaction::ALL {
            assert_eq!(Reaction::parse(reaction.as_str()), Some(reaction));
        }
        assert_eq!(Reaction::parse("angry"), None);
        assert!(serde_json::from_str::<ReactionRequest>(r#"{"reaction":"lol"}"#).is_err());
        let mut counts = ReactionCounts::default();
        counts.add(Reaction::Wow);
        assert_eq!(counts.of(Reaction::Wow), 1);
    }

    #[test]
    fn a_profile_page_reads_as_a_profile_too() {
        let json = r#"{"account":{"id":1,"handle":"Alec"},"about":"Hi","favourites":null,
            "avatar":null,"playtime":null,"trophies":{"bronze":1,"silver":0,"gold":0,"platinum":0},
            "links":[{"provider":"discord","name":"alec#1"}]}"#;
        let page: ProfilePage = serde_json::from_str(json).unwrap();
        assert_eq!(page.profile.about.as_deref(), Some("Hi"));
        assert_eq!(page.links.len(), 1);
        let plain: Profile = serde_json::from_str(json).unwrap();
        assert_eq!(plain, page.profile);
        let bare = r#"{"account":{"id":1,"handle":"A"},"about":null,"favourites":null,
            "avatar":null,"playtime":null}"#;
        let old: ProfilePage = serde_json::from_str(bare).unwrap();
        assert!(old.links.is_empty() && old.trophies.is_none());
    }

    #[test]
    fn a_public_profile_is_off_unless_the_player_turns_it_on() {
        assert!(!PublicProfile::default().public_profile);
        assert_eq!(
            serde_json::to_string(&PublicProfile {
                public_profile: true
            })
            .unwrap(),
            r#"{"public_profile":true}"#
        );
    }

    #[test]
    fn played_with_is_remembered_unless_the_player_turns_it_off() {
        assert!(RememberPlayedWith::default().remember_played_with);
        assert_eq!(
            serde_json::to_string(&RememberPlayedWith {
                remember_played_with: false
            })
            .unwrap(),
            r#"{"remember_played_with":false}"#
        );
        let list: PlayedWithList = serde_json::from_str(
            r#"{"remember":true,"people":[{"account":{"id":4,"handle":"Lumen"},"last":9}]}"#,
        )
        .unwrap();
        let one = &list.people[0];
        assert!(one.title.is_none() && one.title_id.is_none() && !one.friend);
        let text = serde_json::to_string(one).unwrap();
        assert!(!text.contains("title"), "{text}");
    }

    #[test]
    fn privacy_starts_at_friends() {
        assert_eq!(Privacy::default().trophies, Audience::Friends);
        let read: Privacy = serde_json::from_str(r#"{"trophies":"nobody"}"#).unwrap();
        assert_eq!(read.trophies, Audience::Nobody);
    }

    #[test]
    fn privacy_written_before_profiles_still_reads() {
        let read: Privacy = serde_json::from_str(r#"{"trophies":"everyone"}"#).unwrap();
        assert_eq!(read.trophies, Audience::Everyone);
        assert_eq!(read.about, Audience::Friends);
        assert_eq!(read.playtime, Audience::Friends);
        assert_eq!(read.audience(Part::Avatar), Audience::Friends);
        let card: Card =
            serde_json::from_str(r#"{"account":{"id":1,"handle":"Alec"},"trophies":null}"#)
                .unwrap();
        assert_eq!(card.avatar, None);
    }

    #[test]
    fn about_texts_are_plain_and_short() {
        assert_eq!(check_about(""), Ok(()));
        assert_eq!(check_about("Speedruns on weekends, e.g. at night."), Ok(()));
        assert_eq!(check_about("Version 1.5 is my favourite"), Ok(()));
        assert_eq!(check_about(&"a".repeat(MAX_ABOUT)), Ok(()));
        assert_eq!(check_about(&"\u{e4}".repeat(MAX_ABOUT)), Ok(()));
        assert_eq!(
            check_about(&"a".repeat(MAX_ABOUT + 1)),
            Err(AboutProblem::TooLong)
        );
        assert_eq!(check_about("two\nlines"), Err(AboutProblem::Character));
        assert_eq!(check_about("a\u{202E}b"), Err(AboutProblem::Character));
        for link in [
            "see https://example.com",
            "HTTP://x",
            "www.example",
            "visit example.com now",
            "mail me: a@example.org",
            "discord.gg/abc",
            "mailto:a@b.cd",
            "(example.com)",
            "foo.bar-baz.io!",
        ] {
            assert_eq!(check_about(link), Err(AboutProblem::Link), "{link}");
        }
    }

    #[test]
    fn favourites_are_few_distinct_title_ids() {
        let list = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(check_favourites(&[]), Ok(()));
        assert_eq!(
            check_favourites(&list(&["GAME00001", "GAME00002", "GAME00003"])),
            Ok(())
        );
        assert_eq!(
            check_favourites(&list(&["A", "B", "C", "D"])),
            Err(FavouritesProblem::TooMany)
        );
        assert_eq!(
            check_favourites(&list(&["../x"])),
            Err(FavouritesProblem::TitleId)
        );
        assert_eq!(
            check_favourites(&list(&["A", "A"])),
            Err(FavouritesProblem::Duplicate)
        );
    }

    #[test]
    fn days_are_real_utc_dates() {
        assert_eq!(day_number("2000-01-01"), Some(10_957));
        assert_eq!(day_number("2026-10-24"), Some(20_750));
        assert_eq!(day_number("2024-02-29"), Some(19_782));
        assert_eq!(day_number("2025-02-29"), None);
        assert_eq!(day_number("2026-13-01"), None);
        assert_eq!(day_number("2026-00-10"), None);
        assert_eq!(day_number("2026-04-31"), None);
        assert_eq!(day_number("1999-12-31"), None);
        assert_eq!(day_number("2026-1-1"), None);
        assert_eq!(day_number("2026-10-24T00:00"), None);
        assert_eq!(day_number("+026-10-24"), None);
        assert_eq!(day_number("\u{ff12}026-10-24"), None);
    }

    #[test]
    fn avatar_hashes_are_lowercase_hex() {
        assert!(is_avatar_hash(&"a0".repeat(32)));
        assert!(!is_avatar_hash(&"A0".repeat(32)));
        assert!(!is_avatar_hash("abc"));
        assert!(!is_avatar_hash(&format!("../{}", "a".repeat(61))));
    }

    #[test]
    fn handles_follow_the_rules() {
        assert_eq!(check_handle("Alec"), Ok(()));
        assert_eq!(check_handle("a_b-c9"), Ok(()));
        assert_eq!(check_handle("ab"), Err(HandleProblem::Length));
        assert_eq!(
            check_handle("abcdefghijklmnopq"),
            Err(HandleProblem::Length)
        );
        assert_eq!(check_handle("9lives"), Err(HandleProblem::FirstCharacter));
        assert_eq!(check_handle("_under"), Err(HandleProblem::FirstCharacter));
        assert_eq!(check_handle("dot.ted"), Err(HandleProblem::Character));
        assert_eq!(check_handle("Ünïcode"), Err(HandleProblem::FirstCharacter));
        assert_eq!(check_handle("abcdé"), Err(HandleProblem::Character));
    }

    #[test]
    fn the_skeleton_folds_lookalikes_together() {
        // Each row is one skeleton, and every spelling on it passes for the others.
        for (skeleton, spellings) in [
            (
                "flokzy",
                &[
                    "Flokzy",
                    "FIokzy",
                    "F1okzy",
                    "Fiokzy",
                    "Flok_zy",
                    "Flok-zy",
                    "F_l_o_k_z_y",
                ][..],
            ),
            ("foo", &["Foo", "F00", "FoO", "f0o"][..]),
            ("mlke", &["Mike", "rnike", "RNike", "M1ke"][..]),
            ("wolf", &["Wolf", "vvolf", "VVolf"][..]),
        ] {
            for spelling in spellings {
                assert_eq!(handle_skeleton(spelling), skeleton, "{spelling}");
            }
        }
    }

    #[test]
    fn the_skeleton_keeps_apart_what_looks_different() {
        for (a, b) in [
            ("Alec", "Alex"),
            ("sam", "sum"),
            ("mark", "rark"),
            ("vwolf", "wolf"),
            ("s5", "ss"),
            ("zed", "2ed"),
            ("bob", "8ob"),
            ("rm", "m"),
            ("nr", "m"),
        ] {
            assert_ne!(handle_skeleton(a), handle_skeleton(b), "{a} / {b}");
        }
    }

    #[test]
    fn the_skeleton_is_ascii_and_a_fixed_point() {
        for handle in ["Alec", "a_b-c9", "P1ayer_0ne", "rnrnvv"] {
            let skeleton = handle_skeleton(handle);
            assert_eq!(handle_skeleton(&skeleton), skeleton, "{handle}");
            assert!(skeleton.is_ascii());
        }
        assert_eq!(handle_skeleton("P1ayer_0ne"), "playerone");
        assert_eq!(handle_skeleton("rnrnvv"), "mmw");
    }

    #[test]
    fn errors_round_trip_and_tolerate_new_codes() {
        let error = ApiError {
            expected: Some(Box::new(Expected {
                version: "01.100".into(),
                build: "0.1.3".into(),
            })),
            ..ApiError::new(ErrorCode::VersionMismatch, "Versions differ.")
        };
        let text = serde_json::to_string(&error).unwrap();
        assert_eq!(serde_json::from_str::<ApiError>(&text).unwrap(), error);
        let newer: ApiError =
            serde_json::from_str(r#"{"error":"from_the_future","message":"x"}"#).unwrap();
        assert_eq!(newer.error, ErrorCode::Unknown);
        assert_eq!(ErrorCode::VersionMismatch.status(), 409);
    }

    #[test]
    fn client_too_old_carries_the_minimum() {
        assert_eq!(ErrorCode::ClientTooOld.status(), 426);
        assert_eq!(ErrorCode::ClientTooOld.as_str(), "client_too_old");
        let error = ApiError {
            minimum: Some("0.3.0".into()),
            ..ApiError::new(ErrorCode::ClientTooOld, "Update the launcher.")
        };
        let text = serde_json::to_string(&error).unwrap();
        assert_eq!(
            text,
            r#"{"error":"client_too_old","message":"Update the launcher.","minimum":"0.3.0"}"#
        );
        assert_eq!(serde_json::from_str::<ApiError>(&text).unwrap(), error);
        let plain = serde_json::to_string(&ApiError::new(ErrorCode::Internal, "x")).unwrap();
        assert!(!plain.contains("minimum"));
    }

    #[test]
    fn events_are_tagged_by_type() {
        let event: Event =
            serde_json::from_str(r#"{"type":"welcome","address":"198.18.0.7","server_time":5}"#)
                .unwrap();
        assert_eq!(
            event,
            Event::Welcome {
                address: "198.18.0.7".into(),
                server_time: 5
            }
        );
        assert_eq!(
            serde_json::from_str::<Event>(r#"{"type":"friends"}"#).unwrap(),
            Event::Friends
        );
        assert_eq!(
            serde_json::from_str::<Event>(r#"{"type":"comet","tail":1}"#).unwrap(),
            Event::Unknown
        );
        assert_eq!(
            serde_json::to_string(&Event::SignedOut).unwrap(),
            r#"{"type":"signed_out"}"#
        );
        assert_eq!(
            serde_json::from_str::<Event>(r#"{"type":"appear_offline","appear_offline":true}"#)
                .unwrap(),
            Event::AppearOffline {
                appear_offline: true
            }
        );
    }

    #[test]
    fn appear_offline_is_off_unless_the_cloud_says_so() {
        let me: Me = serde_json::from_str(
            r#"{"id":1,"handle":"Lumen","email":"l@example.org","created":5}"#,
        )
        .unwrap();
        assert!(!me.appear_offline);
        assert_eq!(
            serde_json::to_string(&AppearOffline {
                appear_offline: true
            })
            .unwrap(),
            r#"{"appear_offline":true}"#
        );
    }

    #[test]
    fn only_discord_links_are_party_links() {
        for ok in [
            "https://discord.com/channels/1/2",
            "https://discord.gg/nSHbHBRdS",
            "https://discord.com/channels/1/2?x=1",
        ] {
            assert!(is_discord_link(ok), "{ok}");
        }
        for bad in [
            "",
            "http://discord.gg/abc",
            "https://discord.com/",
            "https://discord.com/channels/",
            "https://discord.gg/",
            "https://discord.com.evil.example/channels/1/2",
            "https://discord.com@evil.example/channels/1",
            "https://discord.gg:8443/abc",
            "https://evil.example/https://discord.gg/abc",
            "discord://discord.gg/abc",
            "https://discord.gg/a b",
        ] {
            assert!(!is_discord_link(bad), "{bad}");
        }
    }

    #[test]
    fn calls_are_invitations_that_ring() {
        // Older peers neither send nor know the flag.
        let plain: Invite = serde_json::from_str(r#"{"to":[3]}"#).unwrap();
        assert!(!plain.call);
        assert_eq!(serde_json::to_string(&plain).unwrap(), r#"{"to":[3]}"#);
        let ring = Invite {
            to: vec![AccountId(3)],
            call: true,
        };
        assert_eq!(
            serde_json::to_string(&ring).unwrap(),
            r#"{"to":[3],"call":true}"#
        );
        let invitation: PartyInvitation = serde_json::from_str(
            r#"{"id":"i","from":{"id":1,"handle":"a"},"party":"p","members":1,"expires":9}"#,
        )
        .unwrap();
        assert!(!invitation.call);
        let ended: Event =
            serde_json::from_str(r#"{"type":"call_ended","account":4,"reason":"no_answer"}"#)
                .unwrap();
        assert_eq!(
            ended,
            Event::CallEnded {
                account: AccountId(4),
                reason: CallEnd::NoAnswer
            }
        );
        assert_eq!(
            serde_json::from_str::<CallEnd>(r#""busy""#).unwrap(),
            CallEnd::Unknown
        );
    }

    #[test]
    fn code_answers_are_tagged_by_result() {
        let answer: CodeAnswer =
            serde_json::from_str(r#"{"result":"new_account","signup":"s","expires":9}"#).unwrap();
        assert_eq!(
            answer,
            CodeAnswer::NewAccount {
                signup: "s".into(),
                expires: 9
            }
        );
    }

    #[test]
    fn the_virtual_range_is_198_18_slash_15() {
        assert!(is_virtual("198.18.0.1".parse().unwrap()));
        assert!(is_virtual("198.19.255.254".parse().unwrap()));
        assert!(!is_virtual("198.20.0.1".parse().unwrap()));
        assert!(!is_virtual("192.168.0.1".parse().unwrap()));
    }

    #[test]
    fn slots_count_from_the_start_of_the_range() {
        assert_eq!(virtual_address(0), Some("198.18.0.0".parse().unwrap()));
        assert_eq!(virtual_address(257), Some("198.18.1.1".parse().unwrap()));
        assert_eq!(
            virtual_address(VIRTUAL_ADDRESSES - 1),
            Some("198.19.255.255".parse().unwrap())
        );
        assert_eq!(virtual_address(VIRTUAL_ADDRESSES), None);
        for slot in [0, 1, 255, 256, 70_000, VIRTUAL_ADDRESSES - 1] {
            assert_eq!(virtual_slot(virtual_address(slot).unwrap()), Some(slot));
        }
        assert_eq!(virtual_slot("10.0.0.1".parse().unwrap()), None);
    }

    #[test]
    fn the_ends_of_a_block_are_not_assignable() {
        assert!(is_assignable("198.18.0.1".parse().unwrap()));
        assert!(is_assignable("198.19.255.254".parse().unwrap()));
        assert!(!is_assignable("198.18.0.0".parse().unwrap()));
        assert!(!is_assignable("198.18.7.255".parse().unwrap()));
        assert!(!is_assignable("10.0.0.1".parse().unwrap()));
        let counted = (0..VIRTUAL_ADDRESSES)
            .filter(|s| is_assignable(virtual_address(*s).unwrap()))
            .count() as u32;
        assert_eq!(counted, ASSIGNABLE_ADDRESSES);
    }

    #[test]
    fn points_and_levels_follow_the_adr() {
        let count = Count {
            bronze: 20,
            silver: 8,
            gold: 3,
            platinum: 1,
        };
        assert_eq!(count.points(), 20 * 10 + 8 * 25 + 3 * 60 + 150);
        assert_eq!(Count::default().points(), 0);
        let max = Count {
            bronze: u32::MAX,
            ..Count::default()
        };
        assert_eq!(max.points(), u32::MAX);
        assert_eq!(TrophyLevel::of(0).level, 1);
        assert_eq!(TrophyLevel::of(71).level, 1);
        assert_eq!(TrophyLevel::of(72).level, 2);
        assert_eq!(level_floor(3), 6 * 2 * 13);
        let first = TrophyLevel::of(10);
        assert_eq!((first.from, first.to), (0, Some(72)));
        assert_eq!(level_floor(100), 65_340);
        assert_eq!(TrophyLevel::of(65_339).level, 99);
        let top = TrophyLevel::of(65_340);
        assert_eq!((top.level, top.to), (100, None));
        assert_eq!(TrophyLevel::of(u32::MAX).level, 100);
        // Every level starts where the formula says, and each step is 12 points longer.
        for level in 1..MAX_TROPHY_LEVEL {
            let reached = TrophyLevel::of(level_floor(level + 1));
            assert_eq!(reached.level, level + 1);
            assert_eq!(TrophyLevel::of(level_floor(level + 1) - 1).level, level);
            if level > 1 {
                let step = level_floor(level + 1) - level_floor(level);
                let before = level_floor(level) - level_floor(level - 1);
                assert_eq!(step, before + 12);
            }
        }
    }
}

#[cfg(test)]
mod lfg_and_media_tests {
    use super::*;

    #[test]
    fn a_note_is_short_plain_text() {
        assert!(check_lfg_note("Ranked tonight, mic on").is_ok());
        assert_eq!(
            check_lfg_note(&"x".repeat(MAX_LFG_NOTE + 1)),
            Err(AboutProblem::TooLong)
        );
        assert_eq!(check_lfg_note("join example.com"), Err(AboutProblem::Link));
        assert_eq!(check_lfg_note("a\nb"), Err(AboutProblem::Character));
    }

    #[test]
    fn languages_and_modes_have_names() {
        assert!(is_language_tag("de") && is_language_tag("fil"));
        assert!(!is_language_tag("DE") && !is_language_tag("english") && !is_language_tag(""));
        for mode in [LfgMode::Online, LfgMode::Lan, LfgMode::PlayTogether] {
            assert_eq!(LfgMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(LfgMode::parse("co-op"), None);
        let unknown: LfgMode = serde_json::from_str("\"ranked\"").unwrap();
        assert_eq!(unknown, LfgMode::Unknown);
    }

    #[test]
    fn media_kinds_follow_the_content_type() {
        assert_eq!(
            MediaKind::from_content_type("image/png"),
            Some(MediaKind::Image)
        );
        assert_eq!(
            MediaKind::from_content_type("video/mp4; codecs=avc1"),
            Some(MediaKind::Clip)
        );
        assert_eq!(MediaKind::from_content_type("image/gif"), None);
        assert_eq!(MediaKind::Image.max_bytes(), 10 * 1024 * 1024);
        assert_eq!(MediaKind::Clip.max_bytes(), 100 * 1024 * 1024);
    }
}
