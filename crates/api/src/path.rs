//! The paths of the cloud API. A `{id}` or `{account}` is replaced with [`fill`]; the server
//! routes the same strings.

pub const INFO: &str = "/v1/info";
pub const AUTH_EMAIL: &str = "/v1/auth/email";
pub const AUTH_CODE: &str = "/v1/auth/code";
pub const AUTH_SIGNUP: &str = "/v1/auth/signup";
pub const AUTH_REFRESH: &str = "/v1/auth/refresh";
pub const AUTH_SIGNOUT: &str = "/v1/auth/signout";
pub const AUTH_BROWSER_TOKEN: &str = "/v1/auth/browser/token";
pub const AUTH_BROWSER_LINK: &str = "/v1/auth/browser/link";
/// The page where the browser sign-in starts, on the account web origin.
pub const WEB_AUTHORIZE: &str = "/web/authorize";
pub const ME: &str = "/v1/me";
pub const ME_EXPORT: &str = "/v1/me/export";
pub const ACCOUNTS: &str = "/v1/accounts";
pub const ACCOUNT: &str = "/v1/accounts/{id}";
pub const FRIENDS: &str = "/v1/friends";
pub const FRIEND: &str = "/v1/friends/{id}";
pub const FRIEND_REQUESTS: &str = "/v1/friends/requests";
pub const FRIEND_REQUEST: &str = "/v1/friends/requests/{id}";
pub const FRIEND_REQUEST_ACCEPT: &str = "/v1/friends/requests/{id}/accept";
pub const BLOCK: &str = "/v1/blocks/{id}";
pub const PRESENCE: &str = "/v1/presence";
pub const SESSIONS: &str = "/v1/sessions";
pub const SESSION: &str = "/v1/sessions/{id}";
pub const SESSION_JOIN: &str = "/v1/sessions/{id}/join";
pub const SESSION_LEAVE: &str = "/v1/sessions/{id}/leave";
pub const SESSION_MEMBER: &str = "/v1/sessions/{id}/members/{account}";
pub const SESSION_INVITATIONS: &str = "/v1/sessions/{id}/invitations";
pub const INVITATION: &str = "/v1/invitations/{id}";
pub const RELAY_TICKETS: &str = "/v1/relays/tickets";
pub const REPORTS: &str = "/v1/reports";
pub const MY_TROPHIES: &str = "/v1/me/trophies/{id}";
pub const MY_TROPHIES_SECRET: &str = "/v1/me/trophies/{id}/secret";
pub const MY_TROPHIES_REVEAL: &str = "/v1/me/trophies/{id}/reveal";
pub const COMPARE: &str = "/v1/accounts/{id}/compare/{title}";
pub const ACCOUNT_TROPHIES: &str = "/v1/accounts/{id}/trophies";
pub const RARITY: &str = "/v1/titles/{id}/rarity";
pub const MY_RUN_REPORTS: &str = "/v1/me/run-reports/{id}";
pub const RUNS: &str = "/v1/titles/{id}/runs";
pub const CARDS: &str = "/v1/cards";
pub const PRIVACY: &str = "/v1/me/privacy";
pub const APPEAR_OFFLINE: &str = "/v1/me/appear-offline";
pub const MY_PROFILE: &str = "/v1/me/profile";
pub const ACCOUNT_PROFILE: &str = "/v1/accounts/{id}/profile";
pub const FEED: &str = "/v1/feed";
pub const FEED_REACTION: &str = "/v1/feed/{id}/reaction";
pub const MY_AVATAR: &str = "/v1/me/avatar";
pub const AVATAR: &str = "/v1/avatars/{id}";
pub const MY_PLAYTIME: &str = "/v1/me/playtime";
pub const ME_ACTIONS: &str = "/v1/me/actions";
pub const ME_ACTION_APPEAL: &str = "/v1/me/actions/{id}/appeal";
pub const MOD_REPORTS: &str = "/v1/moderation/reports";
pub const MOD_REPORT: &str = "/v1/moderation/reports/{id}";
pub const MOD_REPORT_DISMISS: &str = "/v1/moderation/reports/{id}/dismiss";
pub const MOD_ACTIONS: &str = "/v1/moderation/actions";
pub const MOD_ACTION_LIFT: &str = "/v1/moderation/actions/{id}/lift";
pub const MOD_APPEALS: &str = "/v1/moderation/appeals";
pub const MOD_APPEAL_DECISION: &str = "/v1/moderation/appeals/{id}/decision";
pub const MOD_LOG: &str = "/v1/moderation/log";
pub const MOD_MODERATORS: &str = "/v1/moderation/moderators";
pub const MOD_MODERATOR: &str = "/v1/moderation/moderators/{id}";
pub const ME_DISCORD_TOKEN: &str = "/v1/me/discord/token";
pub const ME_DISCORD_REFRESH: &str = "/v1/me/discord/refresh";
pub const EVENTS: &str = "/v1/events";
pub const PARTY: &str = "/v1/party";
pub const PARTY_LEAVE: &str = "/v1/party/leave";
pub const PARTY_ENDPOINT: &str = "/v1/party/endpoint";
pub const PARTY_SESSION: &str = "/v1/party/session";
pub const PARTY_MEMBER: &str = "/v1/party/members/{id}";
pub const PARTY_INVITATIONS: &str = "/v1/party/invitations";
pub const PARTY_INVITATION: &str = "/v1/party/invitations/{id}";
pub const PARTY_INVITATION_ACCEPT: &str = "/v1/party/invitations/{id}/accept";
pub const JOIN_CODES: &str = "/v1/join-codes";
pub const JOIN_CODE_CURRENT: &str = "/v1/join-codes/current";
pub const JOIN_CODE_REDEEM: &str = "/v1/join-codes/{id}/redeem";
pub const CALLS_MISSED: &str = "/v1/calls/missed";
pub const CALLS_MISSED_FROM: &str = "/v1/calls/missed/{id}";
pub const CONVERSATIONS: &str = "/v1/conversations";
pub const CONVERSATION: &str = "/v1/conversations/{id}";
pub const CONVERSATION_ACCEPT: &str = "/v1/conversations/{id}/accept";
pub const CONVERSATION_MEMBERS: &str = "/v1/conversations/{id}/members";
pub const CONVERSATION_MESSAGES: &str = "/v1/conversations/{id}/messages";
pub const CONVERSATION_READ: &str = "/v1/conversations/{id}/read";

/// The path with `{id}` replaced by `id`. IDs are numbers or URL-safe tokens, so nothing needs
/// escaping.
pub fn fill(path: &str, id: impl std::fmt::Display) -> String {
    path.replacen("{id}", &id.to_string(), 1)
}

/// The path with `{id}` and `{account}` replaced.
pub fn fill2(path: &str, id: impl std::fmt::Display, account: impl std::fmt::Display) -> String {
    fill(path, id).replacen("{account}", &account.to_string(), 1)
}

/// The path with `{id}` and `{title}` replaced.
pub fn fill_title(path: &str, id: impl std::fmt::Display, title: impl std::fmt::Display) -> String {
    fill(path, id).replacen("{title}", &title.to_string(), 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_filled() {
        assert_eq!(
            fill(FRIEND_REQUEST_ACCEPT, 42),
            "/v1/friends/requests/42/accept"
        );
        assert_eq!(fill2(SESSION_MEMBER, "s1", 7), "/v1/sessions/s1/members/7");
        assert_eq!(
            fill_title(COMPARE, 7, "GAME00001"),
            "/v1/accounts/7/compare/GAME00001"
        );
        assert_eq!(
            fill(CONVERSATION_MESSAGES, "c1"),
            "/v1/conversations/c1/messages"
        );
        assert_eq!(
            fill(PARTY_INVITATION_ACCEPT, "i"),
            "/v1/party/invitations/i/accept"
        );
    }
}
