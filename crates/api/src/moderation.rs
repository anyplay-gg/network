//! Moderation: reports and their queue, actions with a stated reason, what a player sees about an
//! action against them (the statement of reasons), appeals, and the audit log.
//!
//! The model is in `spec/cloud-api.md` ("Moderation") and `docs/adr/0015-moderators-are-accounts.md`.

use serde::{Deserialize, Serialize};

use crate::{Account, AccountId, ReportReason};

/// The longest reason a moderator gives for an action, a dismissal or a decision, in characters.
pub const MAX_REASON: usize = 500;
/// The longest text of an appeal, in characters.
pub const MAX_APPEAL_TEXT: usize = 1000;
/// The longest temporary mute or ban a request can ask for, in days.
pub const MAX_ACTION_DAYS: u64 = 3650;

macro_rules! names {
    ($ty:ident { $($variant:ident => $name:literal),+ $(,)? }) => {
        impl $ty {
            /// The name on the wire, such as `harassment`.
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $name),+ }
            }

            /// The value for a name from [`as_str`](Self::as_str).
            pub fn from_name(name: &str) -> Option<Self> {
                match name { $($name => Some(Self::$variant),)+ _ => None }
            }
        }
    };
}

/// The rule of the terms an action rests on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rule {
    Cheating,
    Harassment,
    Spam,
    Name,
    /// The content or conduct is illegal, not only against the terms.
    Illegal,
    Other,
}

names!(Rule {
    Cheating => "cheating",
    Harassment => "harassment",
    Spam => "spam",
    Name => "name",
    Illegal => "illegal",
    Other => "other",
});

impl From<ReportReason> for Rule {
    fn from(reason: ReportReason) -> Self {
        match reason {
            ReportReason::Cheating => Rule::Cheating,
            ReportReason::Harassment => Rule::Harassment,
            ReportReason::Spam => Rule::Spam,
            ReportReason::Name => Rule::Name,
            ReportReason::Other => Rule::Other,
        }
    }
}

/// What a moderator can do to an account.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    /// A recorded notice. Nothing is restricted.
    Warning,
    /// No friend requests and no messages for a time (`days` is required).
    Mute,
    /// The account is signed out and cannot sign in; `days` absent means for good.
    Ban,
}

names!(ActionKind {
    Warning => "warning",
    Mute => "mute",
    Ban => "ban",
});

/// Where an appeal stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppealStatus {
    /// None filed yet, and the time to file one has not run out.
    Available,
    /// Filed, waiting for a moderator.
    Open,
    /// A moderator decided the action stands.
    Upheld,
    /// A moderator decided the action was wrong; it is lifted.
    Reversed,
    /// None was filed and the time ran out.
    Expired,
}

names!(AppealStatus {
    Available => "available",
    Open => "open",
    Upheld => "upheld",
    Reversed => "reversed",
    Expired => "expired",
});

/// The appeal of one action as the player sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppealInfo {
    pub status: AppealStatus,
    /// Unix seconds. An appeal can be filed until then.
    pub deadline: u64,
    /// What the player wrote, once filed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The moderator's reason for the decision, once decided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided: Option<u64>,
}

/// The statement of reasons: what the player is told about an action against them (DSA Art. 17).
/// It never names who reported them or which moderator acted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Statement {
    pub id: u64,
    pub kind: ActionKind,
    /// The rule of the terms that was broken.
    pub rule: Rule,
    /// The moderator's explanation, in the moderator's words.
    pub reason: String,
    pub created: u64,
    /// Unix seconds when a mute or ban ends. Absent for a warning and for a ban for good.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<u64>,
    /// Unix seconds when the action was taken back, after an appeal or by a moderator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifted: Option<u64>,
    /// True when a player's report started the case.
    pub from_report: bool,
    /// True when software alone decided. The cloud never does.
    pub automated: bool,
    pub appeal: AppealInfo,
}

impl Statement {
    /// True while the action limits the account at `now` (Unix seconds): a mute or a ban that has
    /// neither been lifted nor run out. A warning limits nothing.
    pub fn is_active(&self, now: u64) -> bool {
        self.kind != ActionKind::Warning
            && self.lifted.is_none()
            && self.until.is_none_or(|until| now < until)
    }
}

/// What a banned person gets with the `banned` error: the statement of reasons, when the cloud has
/// one, and a token that reads their statements and files an appeal for a few days
/// (`GET /v1/me/actions`, `POST /v1/me/actions/{id}/appeal`), because a banned account cannot sign
/// in to do it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BanNotice {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statement: Option<Statement>,
    pub appeal_token: String,
}

/// The actions against the signed-in account, newest first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Statements {
    pub actions: Vec<Statement>,
}

/// `POST /v1/me/actions/{id}/appeal`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppealRequest {
    pub text: String,
}

/// Why an account may not do something right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Restriction {
    /// The account is too new to do this yet: write to strangers, or send more friend requests
    /// than a new account may. `until` says when it may.
    NewAccount,
    /// A moderator muted the account. `until` is the end of the mute, absent for good.
    Muted,
}

names!(Restriction {
    NewAccount => "new_account",
    Muted => "muted",
});

// What moderators see and do

/// A moderator, or the operator who authenticates with the admin token (no `id`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<AccountId>,
    /// The moderator's handle when the action was taken, or `operator`.
    pub handle: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportStatus {
    Open,
    Decided,
}

names!(ReportStatus {
    Open => "open",
    Decided => "decided",
});

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportOutcome {
    /// No action: the report was not founded.
    Dismissed,
    /// An action was taken; `action` names it.
    Actioned,
}

names!(ReportOutcome {
    Dismissed => "dismissed",
    Actioned => "actioned",
});

/// A report in the queue, with a snapshot of what was reported. It outlives both accounts until
/// the retention period ends.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportView {
    pub id: u64,
    /// Absent when the reporter deleted their account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reporter: Option<Account>,
    /// The reported account with the handle it had when reported.
    pub reported: Account,
    /// True when the reported account no longer exists.
    pub reported_deleted: bool,
    pub reason: ReportReason,
    pub details: String,
    /// The reported content as the cloud saw it when the report was filed.
    pub content: String,
    pub created: u64,
    pub status: ReportStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<ReportOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<Actor>,
    /// The action that decided the report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<u64>,
}

/// `POST /v1/moderation/actions`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionRequest {
    pub account: AccountId,
    pub kind: ActionKind,
    pub rule: Rule,
    /// 1 to [`MAX_REASON`] characters. The player reads it.
    pub reason: String,
    /// How long a mute or ban lasts. Required for a mute; absent for a ban means for good; not
    /// allowed for a warning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub days: Option<u64>,
    /// The report this action decides. Other open reports about the same account stay open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<u64>,
}

/// `POST /v1/moderation/reports/{id}/dismiss`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dismissal {
    pub reason: String,
}

/// `POST /v1/moderation/actions/{id}/lift`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lifting {
    pub reason: String,
}

/// An action as a moderator sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionView {
    pub account: Account,
    pub actor: Actor,
    pub statement: Statement,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppealOutcome {
    /// The action stands.
    Uphold,
    /// The action was wrong and is lifted.
    Reverse,
}

names!(AppealOutcome {
    Uphold => "uphold",
    Reverse => "reverse",
});

/// `POST /v1/moderation/appeals/{id}/decision`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppealDecision {
    pub outcome: AppealOutcome,
    pub reason: String,
}

/// An appeal in the moderators' list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppealView {
    pub id: u64,
    pub action: ActionView,
    pub created: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<Actor>,
}

/// An account with the moderator role.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Moderator {
    pub account: Account,
    /// Unix seconds when the operator granted the role.
    pub since: u64,
}

/// `POST /v1/moderation/moderators`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantModerator {
    pub handle: String,
}

/// What the audit log records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditAction {
    Warned,
    Muted,
    Banned,
    /// An action was taken back, by a moderator or after an appeal.
    Lifted,
    ReportDismissed,
    AppealFiled,
    AppealUpheld,
    AppealReversed,
    ModeratorGranted,
    ModeratorRevoked,
}

names!(AuditAction {
    Warned => "warned",
    Muted => "muted",
    Banned => "banned",
    Lifted => "lifted",
    ReportDismissed => "report_dismissed",
    AppealFiled => "appeal_filed",
    AppealUpheld => "appeal_upheld",
    AppealReversed => "appeal_reversed",
    ModeratorGranted => "moderator_granted",
    ModeratorRevoked => "moderator_revoked",
});

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceKind {
    Report,
    Action,
    Appeal,
}

names!(ReferenceKind {
    Report => "report",
    Action => "action",
    Appeal => "appeal",
});

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reference {
    pub kind: ReferenceKind,
    pub id: u64,
}

/// One line of the audit log. Lines are only ever added.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: u64,
    pub time: u64,
    pub actor: Actor,
    pub action: AuditAction,
    /// The account the entry is about, with the handle it had then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Account>,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<Rule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<Reference>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_on_the_wire_match_the_serde_names() {
        for rule in [
            Rule::Cheating,
            Rule::Harassment,
            Rule::Spam,
            Rule::Name,
            Rule::Illegal,
            Rule::Other,
        ] {
            assert_eq!(
                serde_json::to_string(&rule).unwrap(),
                format!("\"{}\"", rule.as_str())
            );
            assert_eq!(Rule::from_name(rule.as_str()), Some(rule));
        }
        for action in [
            AuditAction::Warned,
            AuditAction::AppealReversed,
            AuditAction::ModeratorGranted,
        ] {
            assert_eq!(
                serde_json::to_string(&action).unwrap(),
                format!("\"{}\"", action.as_str())
            );
        }
        assert_eq!(Rule::from_name("treason"), None);
    }

    #[test]
    fn a_statement_omits_what_does_not_apply() {
        let statement = Statement {
            id: 3,
            kind: ActionKind::Warning,
            rule: Rule::Spam,
            reason: "Stop posting links.".into(),
            created: 10,
            until: None,
            lifted: None,
            from_report: true,
            automated: false,
            appeal: AppealInfo {
                status: AppealStatus::Available,
                deadline: 99,
                text: None,
                answer: None,
                decided: None,
            },
        };
        let text = serde_json::to_string(&statement).unwrap();
        assert!(!text.contains("until") && !text.contains("lifted"));
        assert_eq!(serde_json::from_str::<Statement>(&text).unwrap(), statement);
    }

    #[test]
    fn only_a_mute_or_a_ban_that_runs_limits_the_account() {
        let action = |kind, until, lifted| Statement {
            id: 1,
            kind,
            rule: Rule::Spam,
            reason: String::new(),
            created: 10,
            until,
            lifted,
            from_report: false,
            automated: false,
            appeal: AppealInfo {
                status: AppealStatus::Available,
                deadline: 99,
                text: None,
                answer: None,
                decided: None,
            },
        };
        assert!(action(ActionKind::Mute, Some(100), None).is_active(99));
        assert!(!action(ActionKind::Mute, Some(100), None).is_active(100));
        assert!(!action(ActionKind::Mute, Some(100), Some(50)).is_active(60));
        assert!(action(ActionKind::Ban, None, None).is_active(u64::MAX));
        assert!(!action(ActionKind::Warning, None, None).is_active(0));
    }

    #[test]
    fn a_report_reason_becomes_a_rule() {
        assert_eq!(Rule::from(ReportReason::Harassment), Rule::Harassment);
        assert_eq!(Rule::from(ReportReason::Other), Rule::Other);
    }
}

#[cfg(test)]
mod wire_tests {
    use crate::{ApiError, ErrorCode, Event};

    use super::*;

    #[test]
    fn a_restricted_error_says_why_and_until_when() {
        let error = ApiError {
            until: Some(1_700_000_000),
            restriction: Some(Restriction::NewAccount),
            ..ApiError::new(ErrorCode::Restricted, "Too new.")
        };
        let text = serde_json::to_string(&error).unwrap();
        assert!(text.contains(r#""error":"restricted""#), "{text}");
        assert!(text.contains(r#""restriction":"new_account""#), "{text}");
        assert_eq!(ErrorCode::Restricted.status(), 403);
        assert_eq!(serde_json::from_str::<ApiError>(&text).unwrap(), error);
    }

    #[test]
    fn a_ban_event_without_a_statement_is_still_read() {
        let old: Event = serde_json::from_str(r#"{"type":"banned","until":5}"#).unwrap();
        assert_eq!(
            old,
            Event::Banned {
                until: Some(5),
                statement: None
            }
        );
        let action: Event = serde_json::from_str(r#"{"type":"action","statement":{"id":1,"kind":"mute","rule":"spam","reason":"x","created":1,"until":9,"from_report":false,"automated":false,"appeal":{"status":"available","deadline":99}}}"#).unwrap();
        assert!(matches!(action, Event::Action { .. }));
    }
}

#[cfg(test)]
mod report_tests {
    use crate::{AccountId, Report, ReportReason};

    #[test]
    fn a_report_about_a_message_names_it_and_an_old_one_still_reads() {
        let old: Report = serde_json::from_str(r#"{"account":5,"reason":"spam"}"#).unwrap();
        assert_eq!((old.conversation, old.seq), (None, None));
        let report = Report {
            account: AccountId(5),
            reason: ReportReason::Harassment,
            details: String::new(),
            conversation: Some("c-1".into()),
            seq: Some(7),
            listing: None,
        };
        let text = serde_json::to_string(&report).unwrap();
        assert!(text.contains(r#""conversation":"c-1""#) && text.contains(r#""seq":7"#));
        assert_eq!(serde_json::from_str::<Report>(&text).unwrap(), report);
    }
}
