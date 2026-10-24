//! The cloud API: paths, request and answer bodies, events and errors.
//!
//! The API is specified in `spec/cloud-api.md`. The net service speaks it as a client and the cloud
//! as a server, so both build on these types and cannot drift apart.

use std::fmt;

use serde::{Deserialize, Serialize};

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
/// The longest name of a group conversation, in characters.
pub const MAX_CONVERSATION_NAME: usize = 64;
/// The most messages one fetch answers with.
pub const MAX_MESSAGES_PAGE: u32 = 100;

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title_id: String,
    pub title: String,
    pub version: String,
    pub build: String,
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
    /// The member's virtual address, such as `198.18.4.20`.
    pub address: String,
    pub joined: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSession {
    pub title_id: String,
    pub title: String,
    pub version: String,
    pub build: String,
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
    pub title_id: String,
    pub title: String,
    pub version: String,
    pub expires: u64,
}

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

/// Trophies by grade.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Count {
    pub bronze: u32,
    pub silver: u32,
    pub gold: u32,
    pub platinum: u32,
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
}

/// A part of a profile that has a visibility of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Part {
    Trophies,
    About,
    Favourites,
    Avatar,
    Playtime,
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
    /// The members or the name of a conversation changed, or there is a new one. Fetch it.
    Conversation {
        id: String,
    },
    /// This account is no longer in the conversation.
    ConversationGone {
        id: String,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<Expected>,
    /// With `restricted`: why. `until` says when the restriction ends, if it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restriction: Option<Restriction>,
    /// With `banned`: why, and how to appeal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ban: Option<Box<BanNotice>>,
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

// Parties

/// A member of a party.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartyMember {
    pub account: Account,
    pub joined: u64,
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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartyInvitation {
    pub id: String,
    pub from: Account,
    pub party: String,
    /// How many members the party has now.
    pub members: u16,
    pub expires: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartyInvitations {
    pub invitations: Vec<PartyInvitation>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateParty {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leader: Option<AccountId>,
}

/// Moves the party into a session of its leader.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveParty {
    pub session: String,
}

// Conversations and messages

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationKind {
    Direct,
    Group,
    Party,
}

/// How far a member read a conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadMark {
    pub account: AccountId,
    pub seq: u64,
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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub conversation: String,
    pub seq: u64,
    pub sender: Account,
    pub text: String,
    pub created: u64,
}

/// Messages in ascending order of `seq`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Messages {
    pub messages: Vec<Message>,
    /// The number of the conversation's newest message.
    pub last: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkRead {
    pub seq: u64,
}

/// True for message text the cloud takes: 1 to [`MAX_MESSAGE_CHARS`] characters, not only white
/// space, and no control characters other than line breaks and tabs.
pub fn is_message_text(text: &str) -> bool {
    let chars = text.chars().count();
    (1..=MAX_MESSAGE_CHARS).contains(&chars)
        && !text.trim().is_empty()
        && !text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn errors_round_trip_and_tolerate_new_codes() {
        let error = ApiError {
            expected: Some(Expected {
                version: "01.100".into(),
                build: "0.1.3".into(),
            }),
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
}
