//! The paths of the cloud API. A `{id}` or `{account}` is replaced with [`fill`]; the server
//! routes the same strings.

pub const INFO: &str = "/v1/info";
pub const AUTH_EMAIL: &str = "/v1/auth/email";
pub const AUTH_CODE: &str = "/v1/auth/code";
pub const AUTH_SIGNUP: &str = "/v1/auth/signup";
pub const AUTH_REFRESH: &str = "/v1/auth/refresh";
pub const AUTH_SIGNOUT: &str = "/v1/auth/signout";
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
pub const ACCOUNT_TROPHIES: &str = "/v1/accounts/{id}/trophies";
pub const RARITY: &str = "/v1/titles/{id}/rarity";
pub const CARDS: &str = "/v1/cards";
pub const PRIVACY: &str = "/v1/me/privacy";
pub const MY_PROFILE: &str = "/v1/me/profile";
pub const ACCOUNT_PROFILE: &str = "/v1/accounts/{id}/profile";
pub const MY_AVATAR: &str = "/v1/me/avatar";
pub const AVATAR: &str = "/v1/avatars/{id}";
pub const MY_PLAYTIME: &str = "/v1/me/playtime";
pub const EVENTS: &str = "/v1/events";

/// The path with `{id}` replaced by `id`. IDs are numbers or URL-safe tokens, so nothing needs
/// escaping.
pub fn fill(path: &str, id: impl std::fmt::Display) -> String {
    path.replacen("{id}", &id.to_string(), 1)
}

/// The path with `{id}` and `{account}` replaced.
pub fn fill2(path: &str, id: impl std::fmt::Display, account: impl std::fmt::Display) -> String {
    fill(path, id).replacen("{account}", &account.to_string(), 1)
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
    }
}
