//! Messages between the launcher and the net service it starts.
//!
//! The protocol is specified in `spec/control-protocol.md`: JSON lines over the net service's
//! standard streams. The launcher has no async runtime, so this crate is plain data and line
//! handling; it does no input or output itself.

use std::collections::BTreeMap;

use anyplay_api::{Account, AccountId, ErrorCode, Expected, FriendLists, Invitation, Visibility};
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
    Email {
        email: String,
    },
    Code {
        code: String,
    },
    SignUp {
        handle: String,
        age_confirmed: bool,
    },
    SignOut,
    DeleteAccount,
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
    },
    Playing {
        game: Option<Game>,
    },
    CheckJoin {
        session: String,
        title_id: String,
        version: String,
        build: String,
    },
    GameStart {
        title_id: String,
        title: String,
        version: String,
        build: String,
        join: Option<String>,
    },
    GameEnd,
    Invite {
        accounts: Vec<AccountId>,
    },
    AnswerInvitation {
        id: String,
        accept: bool,
    },
    Dialog {
        id: u64,
        accounts: Option<Vec<AccountId>>,
    },
    RelayOnly {
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
    Cards {
        accounts: Vec<AccountId>,
    },
    Privacy,
    SetPrivacy {
        privacy: anyplay_api::Privacy,
    },
    Profile {
        account: AccountId,
    },
    UploadPlaytime {
        entries: Vec<anyplay_api::PlaytimeEntry>,
    },
    // Parties and messages
    /// The player's party, or null.
    Party,
    /// Invites friends to the party; makes one when the player is in none.
    PartyInvite {
        accounts: Vec<AccountId>,
    },
    PartyAnswer {
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
    },
    SendMessage {
        conversation: String,
        text: String,
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
    pub expected: Option<Expected>,
}

impl Failure {
    pub const UNKNOWN_COMMAND: &'static str = "unknown_command";
    pub const OFFLINE: &'static str = "offline";
    pub const NOT_SIGNED_IN: &'static str = "not_signed_in";
    pub const NO_GAME: &'static str = "no_game";
    pub const BAD_STATE: &'static str = "bad_state";

    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            expected: None,
        }
    }

    /// The failure that carries a cloud error.
    pub fn api(error: &anyplay_api::ApiError) -> Self {
        Self {
            code: error.error.as_str().to_owned(),
            message: error.message.clone(),
            expected: error.expected.clone(),
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

/// An answer without fields.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Done {}

/// The answer to `party` and `party_answer`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartyState {
    pub party: Option<anyplay_api::Party>,
}

// Events

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
}

/// The signed-in account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedIn {
    pub id: AccountId,
    pub handle: String,
    pub email: String,
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
    Notice {
        code: String,
        message: String,
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
    /// A conversation's members or name changed, or it is new; fetch `conversations`.
    Conversation {
        id: String,
    },
    ConversationGone {
        id: String,
    },
    /// An event this version does not know.
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    #[test]
    fn party_and_message_commands_are_lines() {
        assert_eq!(
            parse_command(r#"{"id":1,"do":"party_invite","accounts":[4,5]}"#),
            Ok((
                1,
                Command::PartyInvite {
                    accounts: vec![AccountId(4), AccountId(5)]
                }
            ))
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
                    limit: None
                }
            ))
        );
        let send = Command::SendMessage {
            conversation: "c".into(),
            text: "hej \u{1F600}".into(),
        };
        assert_eq!(
            parse_command(&command_line(9, &send)),
            Ok((9, send.clone()))
        );
    }

    #[test]
    fn message_events_are_not_taken_for_answers() {
        for event in [
            Event::Message {
                conversation: "c".into(),
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
