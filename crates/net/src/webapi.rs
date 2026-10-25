//! The platform web API groups the net service answers itself (`spec/web-api/`). Every group
//! without a handler here is answered with status 0, "not offered".
//!
//! Handlers are pure: they get the request, the signed-in account and what the account's state
//! is, and return a reply. The service feeds them from state it already holds, so a game that
//! polls ten times a second never causes a request to the cloud.

use std::collections::{BTreeMap, HashMap};
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::path::Path;

use anyplay_api::FriendLists;
use anyplay_bridge_protocol::{WebReply, WebRequest};
use anyplay_control::Restriction;
use serde::Deserialize;

use crate::user_profile;

/// The environment variable that names a test-only file with replacements for the default
/// answers (see [`Override`]). Unset means the defaults. Meant for testers who try answer
/// shapes against a game they own without rebuilding.
pub const OVERRIDES_VAR: &str = "ANYPLAY_NET_WEBAPI_OVERRIDES";

/// How long the restrictions read from the cloud are trusted before they are read again.
pub const RESTRICTIONS_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(60);

/// The API group of a player's communication restriction.
pub const COMMUNICATION_RESTRICTION: &str = "communicationRestrictionStatus";

/// What a handler may look at.
pub struct Context<'a> {
    /// The signed-in account, if the game may act as one.
    pub account: Option<u64>,
    /// The signed-in account's handle, its online name for the game.
    pub handle: &'a str,
    /// The account's friends, as last read from the cloud and kept current by its pushes.
    pub friends: &'a FriendLists,
    /// The restrictions that limit the account, as last read from the cloud.
    pub restrictions: &'a [Restriction],
    /// Unix seconds.
    pub now: u64,
    /// The test-only replacement answers, if the tester set any.
    pub overrides: Option<&'a Path>,
}

/// The reply to a web request, or `None` when the group is not offered.
pub fn answer(web: &WebRequest, ctx: &Context<'_>) -> Option<WebReply> {
    match web.api_group.as_str() {
        COMMUNICATION_RESTRICTION => communication_restriction(web, ctx),
        user_profile::GROUP => user_profile::answer(web, ctx),
        _ => None,
    }
}

/// A short, stable tag for an account id, so that logs tell accounts apart without naming them.
pub fn tag(account: u64) -> String {
    let mut hasher = DefaultHasher::new();
    account.hash(&mut hasher);
    format!("{:08x}", hasher.finish() as u32)
}

fn limits_communication(restriction: &Restriction, now: u64) -> bool {
    (restriction.kind == Restriction::MUTED || restriction.kind == Restriction::BANNED)
        && restriction.until.is_none_or(|until| now < until)
}

/// True while a mute or a ban that has not run out keeps the account from communicating. A new
/// account's limit on strangers is not one: the account may still play.
pub fn communication_restricted(restrictions: &[Restriction], now: u64) -> bool {
    restrictions.iter().any(|r| limits_communication(r, now))
}

/// When the restriction ends: the latest end, or none when one of them is for good.
fn restricted_until(restrictions: &[Restriction], now: u64) -> Option<u64> {
    restrictions
        .iter()
        .filter(|r| limits_communication(r, now))
        .try_fold(0u64, |latest, r| r.until.map(|until| latest.max(until)))
}

/// `GET /v3/users/{account}/communication/restriction/status`, for the signed-in account only.
fn communication_restriction(web: &WebRequest, ctx: &Context<'_>) -> Option<WebReply> {
    let path = web.path.split('?').next().unwrap_or_default();
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    let [
        "v3",
        "users",
        user,
        "communication",
        "restriction",
        "status",
    ] = parts[..]
    else {
        return None;
    };
    if !web.method.eq_ignore_ascii_case("GET") {
        return None;
    }
    let me = ctx.account?;
    if user != "me" && user.parse::<u64>().ok() != Some(me) {
        // Another player's state is not ours to tell.
        tracing::debug!(
            group = COMMUNICATION_RESTRICTION,
            "other account, not offered"
        );
        return None;
    }
    let restricted = communication_restricted(ctx.restrictions, ctx.now);
    let mut default = serde_json::json!({ "restricted": restricted });
    if restricted && let Some(until) = restricted_until(ctx.restrictions, ctx.now) {
        default["until"] = until.into();
    }
    let state = if restricted {
        "restricted"
    } else {
        "notRestricted"
    };
    let replacement = read_overrides(ctx.overrides)
        .and_then(|mut groups| groups.remove(COMMUNICATION_RESTRICTION)?.remove(state));
    let overridden = replacement.is_some();
    let reply = replacement
        .map(Override::into_reply)
        .unwrap_or_else(|| json_reply(200, &default));
    tracing::debug!(
        group = COMMUNICATION_RESTRICTION,
        account = tag(me),
        restricted,
        status = reply.status,
        overridden,
        "answered"
    );
    Some(reply)
}

pub(crate) fn json_reply(status: u16, body: &serde_json::Value) -> WebReply {
    WebReply {
        status,
        headers: vec![("Content-Type".into(), "application/json".into())],
        body: serde_json::to_vec(body).unwrap_or_default(),
    }
}

/// One replacement reply in the test-only override file, which maps API group, then state, to
/// the whole reply:
///
/// ```json
/// { "communicationRestrictionStatus": {
///     "notRestricted": { "status": 200, "headers": {"Content-Type": "application/json"},
///                        "body": {"restricted": false} },
///     "restricted":    { "status": 200, "body_text": "..." } } }
/// ```
///
/// `status` defaults to 200, `headers` to a JSON content type, `body` is any JSON value and
/// `body_text` is sent as it is (for an empty or non-JSON body). The file is read on every
/// request, so a tester edits it while the game runs.
#[derive(Deserialize)]
pub(crate) struct Override {
    status: Option<u16>,
    headers: Option<BTreeMap<String, String>>,
    body: Option<serde_json::Value>,
    body_text: Option<String>,
    /// How the default answer of `userProfile` is shaped (see [`user_profile::Shape`]).
    pub(crate) shape: Option<user_profile::Shape>,
}

type Overrides = HashMap<String, HashMap<String, Override>>;

pub(crate) fn read_overrides(path: Option<&Path>) -> Option<Overrides> {
    let path = path?;
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            tracing::warn!("{OVERRIDES_VAR}: cannot read {}: {error}", path.display());
            return None;
        }
    };
    match serde_json::from_str(&text) {
        Ok(overrides) => Some(overrides),
        Err(error) => {
            tracing::warn!("{OVERRIDES_VAR}: {} is not valid: {error}", path.display());
            None
        }
    }
}

impl Override {
    /// True when the entry gives a whole reply, not only a shape for the default one.
    pub(crate) fn replaces_reply(&self) -> bool {
        self.status.is_some()
            || self.headers.is_some()
            || self.body.is_some()
            || self.body_text.is_some()
    }

    fn into_reply(self) -> WebReply {
        self.into_reply_with(&[])
    }

    /// The reply, with each `{name}` of `body_text` replaced by its value.
    pub(crate) fn into_reply_with(self, values: &[(&str, String)]) -> WebReply {
        let body = match (self.body_text, self.body) {
            (Some(text), _) => values
                .iter()
                .fold(text, |text, (name, value)| {
                    text.replace(&format!("{{{name}}}"), value)
                })
                .into_bytes(),
            (None, Some(value)) => serde_json::to_vec(&value).unwrap_or_default(),
            (None, None) => Vec::new(),
        };
        let headers = match self.headers {
            Some(headers) => headers.into_iter().collect(),
            None => vec![("Content-Type".into(), "application/json".into())],
        };
        WebReply {
            status: self.status.unwrap_or(200),
            headers,
            body,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static NO_FRIENDS: FriendLists = FriendLists {
        friends: Vec::new(),
        incoming: Vec::new(),
        outgoing: Vec::new(),
        blocked: Vec::new(),
    };

    const PATH: &str = "/v3/users/42/communication/restriction/status";

    fn request(path: &str) -> WebRequest {
        WebRequest {
            user_id: 0,
            api_group: COMMUNICATION_RESTRICTION.into(),
            path: path.into(),
            method: "GET".into(),
            headers: vec![("X-Psn-NpCppWebApi-Context".into(), "PS5/1.00".into())],
            body: Vec::new(),
        }
    }

    fn mute(until: Option<u64>) -> Restriction {
        Restriction {
            until,
            ..Restriction::new(Restriction::MUTED, None)
        }
    }

    fn ctx<'a>(restrictions: &'a [Restriction], overrides: Option<&'a Path>) -> Context<'a> {
        Context {
            account: Some(42),
            handle: "me",
            friends: &NO_FRIENDS,
            restrictions,
            now: 1000,
            overrides,
        }
    }

    fn body(reply: &WebReply) -> serde_json::Value {
        serde_json::from_slice(&reply.body).unwrap()
    }

    #[test]
    fn an_account_without_restrictions_is_not_restricted() {
        let reply = answer(&request(PATH), &ctx(&[], None)).unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(body(&reply), serde_json::json!({"restricted": false}));
    }

    #[test]
    fn a_mute_restricts_until_it_ends() {
        let mutes = [mute(Some(2000))];
        let reply = answer(&request(PATH), &ctx(&mutes, None)).unwrap();
        assert_eq!(
            body(&reply),
            serde_json::json!({"restricted": true, "until": 2000})
        );
        let mut late = ctx(&mutes, None);
        late.now = 2000;
        let reply = answer(&request(PATH), &late).unwrap();
        assert_eq!(body(&reply), serde_json::json!({"restricted": false}));
    }

    #[test]
    fn a_mute_for_good_has_no_end() {
        let mutes = [mute(Some(2000)), mute(None)];
        let reply = answer(&request(PATH), &ctx(&mutes, None)).unwrap();
        assert_eq!(body(&reply), serde_json::json!({"restricted": true}));
    }

    #[test]
    fn only_mutes_and_bans_restrict() {
        let other = [Restriction::new("new_account", Some(5000))];
        assert!(!communication_restricted(&other, 1000));
        let ban = [Restriction::new(Restriction::BANNED, None)];
        assert!(communication_restricted(&ban, 1000));
    }

    #[test]
    fn the_query_and_me_are_accepted_and_other_accounts_are_not() {
        assert!(answer(&request(&format!("{PATH}?")), &ctx(&[], None)).is_some());
        let me = request("/v3/users/me/communication/restriction/status");
        assert!(answer(&me, &ctx(&[], None)).is_some());
        let other = request("/v3/users/43/communication/restriction/status");
        assert!(answer(&other, &ctx(&[], None)).is_none());
    }

    #[test]
    fn signed_out_other_paths_other_methods_and_other_groups_are_not_offered() {
        let mut signed_out = ctx(&[], None);
        signed_out.account = None;
        assert!(answer(&request(PATH), &signed_out).is_none());
        let elsewhere = request("/v3/users/42/communication/other");
        assert!(answer(&elsewhere, &ctx(&[], None)).is_none());
        let mut post = request(PATH);
        post.method = "POST".into();
        assert!(answer(&post, &ctx(&[], None)).is_none());
        let mut group = request(PATH);
        group.api_group = "userProfile".into();
        assert!(answer(&group, &ctx(&[], None)).is_none());
    }

    #[test]
    fn an_override_file_replaces_the_answer_per_state() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("o.json");
        std::fs::write(
            &file,
            r#"{"communicationRestrictionStatus": {
                "notRestricted": {"status": 204, "body_text": ""},
                "restricted": {"body": {"status": "RESTRICTED"}}}}"#,
        )
        .unwrap();
        let free = answer(&request(PATH), &ctx(&[], Some(&file))).unwrap();
        assert_eq!(free.status, 204);
        assert!(free.body.is_empty());
        let mutes = [mute(None)];
        let held = answer(&request(PATH), &ctx(&mutes, Some(&file))).unwrap();
        assert_eq!(held.status, 200);
        assert_eq!(body(&held), serde_json::json!({"status": "RESTRICTED"}));
    }

    #[test]
    fn a_broken_or_missing_override_file_falls_back_to_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let broken = dir.path().join("broken.json");
        std::fs::write(&broken, "{").unwrap();
        for file in [broken, dir.path().join("missing.json")] {
            let reply = answer(&request(PATH), &ctx(&[], Some(&file))).unwrap();
            assert_eq!(body(&reply), serde_json::json!({"restricted": false}));
        }
    }

    #[test]
    fn the_tag_is_stable_and_tells_accounts_apart() {
        assert_eq!(tag(42), tag(42));
        assert_ne!(tag(42), tag(43));
        assert_eq!(tag(42).len(), 8);
    }
}
