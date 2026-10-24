//! The cloud API: paths, request and answer bodies, events and errors.
//!
//! The API is specified in `spec/cloud-api.md`. The net service speaks it as a client and the cloud
//! as a server, so both build on these types and cannot drift apart.

use std::fmt;

use serde::{Deserialize, Serialize};

pub mod path;

/// The largest request body the cloud reads.
pub const MAX_BODY: usize = 64 * 1024;
/// The largest session properties, once decoded.
pub const MAX_PROPERTIES: usize = 8 * 1024;
/// The largest number of members a session can have.
pub const MAX_MEMBERS: u16 = 64;
/// The longest report details, in characters.
pub const MAX_REPORT_DETAILS: usize = 1000;
/// Account IDs stay below 2^53 so every JSON parser reads them exactly.
pub const MAX_ACCOUNT_ID: u64 = (1 << 53) - 1;
/// The shortest and longest handle, in characters.
pub const HANDLE_LEN: std::ops::RangeInclusive<usize> = 3..=16;

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
    Banned {
        until: Option<u64>,
    },
    /// An event this version does not know.
    #[serde(other)]
    Unknown,
}

/// Close codes of the event connection.
pub mod close {
    pub const TOKEN_EXPIRED: u16 = 4001;
    pub const BANNED: u16 = 4003;
    pub const SIGNED_OUT: u16 = 4004;
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
            ErrorCode::Banned | ErrorCode::Forbidden => 403,
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
}

impl ApiError {
    pub fn new(error: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            error,
            message: message.into(),
            until: None,
            expected: None,
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

#[cfg(test)]
mod tests {
    use super::*;

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
