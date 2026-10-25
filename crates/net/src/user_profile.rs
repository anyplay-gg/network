//! The web API group `userProfile` ([`spec/web-api/userProfile.md`](../../../spec/web-api/userProfile.md)):
//! the signed-in player's friends and the profiles of the player and those friends, from the
//! AnyPlay friend list the service already holds. Nothing here asks the cloud.
//!
//! The id a game sees for an account is the AnyPlay account number itself, for the player and
//! for every friend, so both players' PCs agree on it.

use std::collections::HashSet;

use anyplay_api::FriendLists;
use anyplay_bridge_protocol::{WebReply, WebRequest};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::webapi::{Context, Override, json_reply, read_overrides, tag};

/// The API group.
pub const GROUP: &str = "userProfile";

/// The most friends one answer lists, and the limit when the request names none.
pub const MAX_LIMIT: usize = 2000;

/// The most profiles one request is answered for; further ids are left out.
pub const MAX_PROFILES: usize = 100;

/// Override keys (see the spec): the two calls of the group.
const FRIENDS: &str = "friends";
const PROFILES: &str = "profiles";

/// How an id is written.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IdForm {
    /// `"123"`.
    #[default]
    String,
    /// `123`.
    Number,
}

/// What a friend is in a list.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Items {
    /// Just the id.
    #[default]
    Ids,
    /// An object with the id and the online name, like a profile.
    Objects,
}

/// The shape of the default answers. The names are our own guess; a tester changes them in the
/// override file until a game accepts the answer. A key set to `""` is left out.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Shape {
    pub ids: IdForm,
    pub items: Items,
    /// The array of friends in the `friends` answer.
    pub friends_key: String,
    /// The array of profiles in the `profiles` answer.
    pub profiles_key: String,
    /// The id of a profile or of a friend written as an object.
    pub id_key: String,
    /// The online name (the AnyPlay handle) of a profile.
    pub name_key: String,
    pub offset_key: String,
    pub limit_key: String,
    pub total_key: String,
}

impl Default for Shape {
    fn default() -> Self {
        Self {
            ids: IdForm::default(),
            items: Items::default(),
            friends_key: "friends".into(),
            profiles_key: "profiles".into(),
            id_key: "accountId".into(),
            name_key: "onlineId".into(),
            offset_key: "offset".into(),
            limit_key: "limit".into(),
            total_key: "total".into(),
        }
    }
}

impl Shape {
    fn id(&self, id: u64) -> Value {
        match self.ids {
            IdForm::String => Value::String(id.to_string()),
            IdForm::Number => Value::from(id),
        }
    }

    fn profile(&self, id: u64, handle: &str) -> Value {
        let mut object = Map::new();
        put(&mut object, &self.id_key, self.id(id));
        put(&mut object, &self.name_key, Value::from(handle));
        Value::Object(object)
    }
}

fn put(object: &mut Map<String, Value>, key: &str, value: Value) {
    if !key.is_empty() {
        object.insert(key.to_owned(), value);
    }
}

/// An account the game may see.
struct Seen {
    id: u64,
    handle: String,
}

/// The reply to a `userProfile` request, or `None` when the request is not offered.
pub fn answer(web: &WebRequest, ctx: &Context<'_>) -> Option<WebReply> {
    if !web.method.eq_ignore_ascii_case("GET") {
        return None;
    }
    let me = ctx.account?;
    let (path, query) = web.path.split_once('?').unwrap_or((web.path.as_str(), ""));
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    match parts[..] {
        ["v1", "users", user, "friends"] => {
            if user != "me" && user.parse::<u64>().ok() != Some(me) {
                // Another player's friends are not ours to tell.
                tracing::debug!(
                    target: "anyplay_net::webapi",
                    group = GROUP,
                    "other account, not offered"
                );
                return None;
            }
            Some(friends(me, ctx, query))
        }
        ["v1", "users", "profiles"] => Some(profiles(me, ctx, query)),
        _ => None,
    }
}

/// The friends the game may see, in a stable order that both players' PCs would agree on for
/// the same list: by handle without regard to case, then by id. Blocked accounts and the player
/// themself never appear.
fn visible_friends(me: u64, lists: &FriendLists) -> Vec<Seen> {
    let blocked: HashSet<u64> = lists.blocked.iter().map(|a| a.id.0).collect();
    let mut seen = HashSet::new();
    let mut friends: Vec<Seen> = lists
        .friends
        .iter()
        .map(|f| &f.account)
        .filter(|a| a.id.0 != me && !blocked.contains(&a.id.0) && seen.insert(a.id.0))
        .map(|a| Seen {
            id: a.id.0,
            handle: a.handle.clone(),
        })
        .collect();
    friends.sort_by_cached_key(|f| (f.handle.to_lowercase(), f.id));
    friends
}

fn friends(me: u64, ctx: &Context<'_>, query: &str) -> WebReply {
    let all = visible_friends(me, ctx.friends);
    let offset = number(query, "offset").unwrap_or(0);
    let limit = number(query, "limit").map_or(MAX_LIMIT, |l| l.min(MAX_LIMIT));
    let page: Vec<&Seen> = all.iter().skip(offset).take(limit).collect();
    let (shape, replacement) = shape_of(ctx, FRIENDS);
    let items: Vec<Value> = page
        .iter()
        .map(|f| match shape.items {
            Items::Ids => shape.id(f.id),
            Items::Objects => shape.profile(f.id, &f.handle),
        })
        .collect();
    let mut body = Map::new();
    put(&mut body, &shape.friends_key, Value::Array(items));
    put(&mut body, &shape.offset_key, offset.into());
    put(&mut body, &shape.limit_key, limit.into());
    put(&mut body, &shape.total_key, all.len().into());
    let values = [
        ("account", me.to_string()),
        ("handle", ctx.handle.to_owned()),
        ("total", all.len().to_string()),
        ("count", page.len().to_string()),
        ("offset", offset.to_string()),
        ("limit", limit.to_string()),
        (
            "ids",
            Value::Array(page.iter().map(|f| shape.id(f.id)).collect()).to_string(),
        ),
        (
            "profiles",
            Value::Array(
                page.iter()
                    .map(|f| shape.profile(f.id, &f.handle))
                    .collect(),
            )
            .to_string(),
        ),
    ];
    reply(
        me,
        FRIENDS,
        page.len(),
        replacement,
        &Value::Object(body),
        &values,
    )
}

fn profiles(me: u64, ctx: &Context<'_>, query: &str) -> WebReply {
    let friends = visible_friends(me, ctx.friends);
    let mut shown: Vec<Seen> = Vec::new();
    let mut asked = HashSet::new();
    for id in values(query, "accountIds")
        .iter()
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        let id = if id == "me" {
            Some(me)
        } else {
            id.parse::<u64>().ok()
        };
        // Someone who is neither the player nor a friend, or who is not an account at all, is
        // left out: the answer does not say whether the account exists.
        let Some(id) = id else { continue };
        let handle = if id == me {
            ctx.handle.to_owned()
        } else if let Some(friend) = friends.iter().find(|f| f.id == id) {
            friend.handle.clone()
        } else {
            continue;
        };
        if asked.insert(id) && shown.len() < MAX_PROFILES {
            shown.push(Seen { id, handle });
        }
    }
    let (shape, replacement) = shape_of(ctx, PROFILES);
    let list: Vec<Value> = shown
        .iter()
        .map(|p| shape.profile(p.id, &p.handle))
        .collect();
    let mut body = Map::new();
    put(&mut body, &shape.profiles_key, Value::Array(list.clone()));
    let values = [
        ("account", me.to_string()),
        ("handle", ctx.handle.to_owned()),
        ("total", shown.len().to_string()),
        ("count", shown.len().to_string()),
        (
            "ids",
            Value::Array(shown.iter().map(|p| shape.id(p.id)).collect()).to_string(),
        ),
        ("profiles", Value::Array(list).to_string()),
    ];
    reply(
        me,
        PROFILES,
        shown.len(),
        replacement,
        &Value::Object(body),
        &values,
    )
}

/// The shape to use and the tester's replacement for the call, if there is one.
fn shape_of(ctx: &Context<'_>, call: &str) -> (Shape, Option<Override>) {
    let replacement =
        read_overrides(ctx.overrides).and_then(|mut groups| groups.remove(GROUP)?.remove(call));
    let shape = replacement
        .as_ref()
        .and_then(|r| r.shape.clone())
        .unwrap_or_default();
    (shape, replacement)
}

fn reply(
    me: u64,
    call: &str,
    count: usize,
    replacement: Option<Override>,
    default: &Value,
    values: &[(&str, String)],
) -> WebReply {
    let overridden = replacement.as_ref().is_some_and(Override::replaces_reply);
    let reply = match replacement {
        Some(replacement) if replacement.replaces_reply() => replacement.into_reply_with(values),
        _ => json_reply(200, default),
    };
    tracing::debug!(
        target: "anyplay_net::webapi",
        group = GROUP,
        call,
        account = tag(me),
        count,
        status = reply.status,
        overridden,
        "answered"
    );
    reply
}

/// The values a query parameter has, percent-decoded, in the order they appear.
fn values(query: &str, name: &str) -> Vec<String> {
    query
        .split('&')
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(key) == name).then(|| decode(value))
        })
        .collect()
}

/// A query parameter that is a plain non-negative number. `None` when it is absent, empty or
/// not a number, so that the default applies.
fn number(query: &str, name: &str) -> Option<usize> {
    values(query, name).last()?.trim().parse().ok()
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                (Some(high), Some(low)) => {
                    out.push(high * 16 + low);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(byte: u8) -> Option<u8> {
    (byte as char).to_digit(16).map(|d| d as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyplay_api::{Account, AccountId, Friend, Presence};
    use serde_json::json;

    fn account(id: u64, handle: &str) -> Account {
        Account {
            id: AccountId(id),
            handle: handle.into(),
        }
    }

    fn lists(friends: &[(u64, &str)], blocked: &[(u64, &str)]) -> FriendLists {
        FriendLists {
            friends: friends
                .iter()
                .map(|(id, handle)| Friend {
                    account: account(*id, handle),
                    since: 0,
                    presence: Presence::default(),
                })
                .collect(),
            incoming: Vec::new(),
            outgoing: Vec::new(),
            blocked: blocked.iter().map(|(id, h)| account(*id, h)).collect(),
        }
    }

    fn ctx<'a>(lists: &'a FriendLists, overrides: Option<&'a std::path::Path>) -> Context<'a> {
        Context {
            account: Some(42),
            handle: "Me",
            friends: lists,
            restrictions: &[],
            now: 1000,
            overrides,
        }
    }

    fn request(path: &str) -> WebRequest {
        WebRequest {
            user_id: 0,
            api_group: GROUP.into(),
            path: path.into(),
            method: "GET".into(),
            headers: vec![("X-Psn-NpCppWebApi-Context".into(), "PS5/1.00".into())],
            body: Vec::new(),
        }
    }

    fn body(reply: &WebReply) -> Value {
        serde_json::from_slice(&reply.body).unwrap()
    }

    const FRIENDS_PATH: &str = "/v1/users/42/friends?offset=0&limit=2000&filter=&order=";

    #[test]
    fn friends_are_listed_by_their_anyplay_account_ids_in_a_stable_order() {
        let l = lists(&[(7, "zed"), (9, "Amy"), (8, "amy")], &[]);
        let reply = answer(&request(FRIENDS_PATH), &ctx(&l, None)).unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(
            body(&reply),
            json!({"friends": ["8", "9", "7"], "offset": 0, "limit": 2000, "total": 3})
        );
    }

    #[test]
    fn the_ids_are_the_account_numbers_the_bridge_reports() {
        // The same number is the bridge's `account_id` for a player and the id of that player
        // in a friend's list on the other PC.
        let mine = lists(&[(43, "bob")], &[]);
        let theirs = lists(&[(42, "me")], &[]);
        let a = answer(&request("/v1/users/me/friends"), &ctx(&mine, None)).unwrap();
        assert_eq!(body(&a)["friends"], json!(["43"]));
        let mut as_bob = ctx(&theirs, None);
        as_bob.account = Some(43);
        let b = answer(&request("/v1/users/43/friends"), &as_bob).unwrap();
        assert_eq!(body(&b)["friends"], json!(["42"]));
    }

    #[test]
    fn friends_are_paged_by_offset_and_limit() {
        let l = lists(&[(1, "a"), (2, "b"), (3, "c"), (4, "d")], &[]);
        let page = |q: &str| {
            let reply = answer(
                &request(&format!("/v1/users/42/friends?{q}")),
                &ctx(&l, None),
            );
            body(&reply.unwrap())
        };
        assert_eq!(page("offset=1&limit=2")["friends"], json!(["2", "3"]));
        assert_eq!(page("offset=1&limit=2")["total"], 4);
        assert_eq!(page("offset=3&limit=2")["friends"], json!(["4"]));
        assert_eq!(page("offset=9&limit=2")["friends"], json!([]));
        assert_eq!(page("limit=0")["friends"], json!([]));
        // No or unusable paging means the defaults.
        assert_eq!(page("")["friends"].as_array().unwrap().len(), 4);
        assert_eq!(
            page("offset=x&limit=")["friends"].as_array().unwrap().len(),
            4
        );
        assert_eq!(page("limit=999999")["limit"], MAX_LIMIT);
    }

    #[test]
    fn blocked_accounts_and_the_player_never_appear() {
        let l = lists(&[(43, "bob"), (44, "eve"), (42, "me")], &[(44, "eve")]);
        let reply = answer(&request("/v1/users/42/friends"), &ctx(&l, None)).unwrap();
        assert_eq!(body(&reply)["friends"], json!(["43"]));
        assert_eq!(body(&reply)["total"], 1);
        let asked = answer(&request("/v1/users/profiles?accountIds=44"), &ctx(&l, None)).unwrap();
        assert_eq!(body(&asked)["profiles"], json!([]));
    }

    #[test]
    fn profiles_show_the_player_and_friends_with_the_handle_as_online_name() {
        let l = lists(&[(43, "bob")], &[]);
        let reply = answer(
            &request("/v1/users/profiles?accountIds=42%2C43,43,me"),
            &ctx(&l, None),
        )
        .unwrap();
        assert_eq!(
            body(&reply),
            json!({"profiles": [
                {"accountId": "42", "onlineId": "Me"},
                {"accountId": "43", "onlineId": "bob"}]})
        );
    }

    #[test]
    fn profiles_of_strangers_and_non_numbers_are_left_out() {
        let l = lists(&[(43, "bob")], &[]);
        let c = ctx(&l, None);
        for q in [
            "accountIds=99",
            "accountIds=abc",
            "accountIds=",
            "",
            "other=42",
        ] {
            let reply = answer(&request(&format!("/v1/users/profiles?{q}")), &c).unwrap();
            assert_eq!(reply.status, 200, "{q}");
            assert_eq!(body(&reply)["profiles"], json!([]), "{q}");
        }
        let repeated = answer(
            &request("/v1/users/profiles?accountIds=42&accountIds=43&accountIds=99"),
            &c,
        )
        .unwrap();
        assert_eq!(body(&repeated)["profiles"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn a_request_is_answered_for_at_most_a_hundred_profiles() {
        let many: Vec<(u64, String)> = (100..400).map(|i| (i, format!("p{i}"))).collect();
        let refs: Vec<(u64, &str)> = many.iter().map(|(i, h)| (*i, h.as_str())).collect();
        let l = lists(&refs, &[]);
        let ids: Vec<String> = (100..400).map(|i: u64| i.to_string()).collect();
        let path = format!("/v1/users/profiles?accountIds={}", ids.join(","));
        let reply = answer(&request(&path), &ctx(&l, None)).unwrap();
        assert_eq!(
            body(&reply)["profiles"].as_array().unwrap().len(),
            MAX_PROFILES
        );
    }

    #[test]
    fn only_the_signed_in_player_asking_by_get_is_answered() {
        let l = lists(&[(43, "bob")], &[]);
        let c = ctx(&l, None);
        assert!(answer(&request("/v1/users/43/friends"), &c).is_none());
        assert!(answer(&request("/v1/users/local_268435456/friends?"), &c).is_none());
        assert!(answer(&request("/v1/users/42/other"), &c).is_none());
        assert!(answer(&request("/v1/users/42/friends/extra"), &c).is_none());
        let mut post = request(FRIENDS_PATH);
        post.method = "POST".into();
        assert!(answer(&post, &c).is_none());
        let mut out = ctx(&l, None);
        out.account = None;
        assert!(answer(&request(FRIENDS_PATH), &out).is_none());
        assert!(answer(&request("/v1/users/profiles?accountIds=42"), &out).is_none());
    }

    #[test]
    fn the_dispatcher_routes_the_group_here() {
        let l = lists(&[(43, "bob")], &[]);
        let reply = crate::webapi::answer(&request(FRIENDS_PATH), &ctx(&l, None)).unwrap();
        assert_eq!(body(&reply)["friends"], json!(["43"]));
        let mut other = request(FRIENDS_PATH);
        other.api_group = "somethingElse".into();
        assert!(crate::webapi::answer(&other, &ctx(&l, None)).is_none());
    }

    fn overrides(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("o.json");
        std::fs::write(&file, text).unwrap();
        (dir, file)
    }

    #[test]
    fn a_shape_override_renames_and_retypes_the_default_answer() {
        let (_dir, file) = overrides(
            r#"{"userProfile": {
                "friends": {"shape": {"items": "objects", "ids": "number",
                    "friends_key": "list", "id_key": "id", "name_key": "name",
                    "limit_key": "", "offset_key": "start", "total_key": "totalItemCount"}},
                "profiles": {"shape": {"profiles_key": "items", "id_key": "id", "name_key": "name"}}}}"#,
        );
        let l = lists(&[(43, "bob")], &[]);
        let c = ctx(&l, Some(&file));
        let f = answer(&request(FRIENDS_PATH), &c).unwrap();
        assert_eq!(
            body(&f),
            json!({"list": [{"id": 43, "name": "bob"}], "start": 0, "totalItemCount": 1})
        );
        let p = answer(&request("/v1/users/profiles?accountIds=43"), &c).unwrap();
        assert_eq!(body(&p), json!({"items": [{"id": "43", "name": "bob"}]}));
    }

    #[test]
    fn a_whole_reply_override_can_use_the_live_values() {
        let (_dir, file) = overrides(
            r#"{"userProfile": {
                "friends": {"status": 200, "body_text": "{\"n\":{total},\"f\":{ids}}"},
                "profiles": {"status": 204, "body_text": ""}}}"#,
        );
        let l = lists(&[(43, "bob")], &[]);
        let c = ctx(&l, Some(&file));
        let f = answer(&request(FRIENDS_PATH), &c).unwrap();
        assert_eq!(
            String::from_utf8(f.body).unwrap(),
            "{\"n\":1,\"f\":[\"43\"]}"
        );
        let p = answer(&request("/v1/users/profiles?accountIds=42"), &c).unwrap();
        assert_eq!(p.status, 204);
        assert!(p.body.is_empty());
    }

    #[test]
    fn a_broken_override_file_or_another_group_leaves_the_default() {
        let (_dir, broken) = overrides("{");
        let l = lists(&[(43, "bob")], &[]);
        let f = answer(&request(FRIENDS_PATH), &ctx(&l, Some(&broken))).unwrap();
        assert_eq!(body(&f)["friends"], json!(["43"]));
        let (_dir2, other) =
            overrides(r#"{"communicationRestrictionStatus": {"restricted": {"status": 500}}}"#);
        let f = answer(&request(FRIENDS_PATH), &ctx(&l, Some(&other))).unwrap();
        assert_eq!(f.status, 200);
    }

    #[test]
    fn percent_decoding_keeps_what_is_not_an_escape() {
        assert_eq!(decode("a%2Cb+c%zz%4"), "a,b c%zz%4");
        assert_eq!(
            values("x=1&accountIds=%34%32&accountIds=7", "accountIds"),
            ["42", "7"]
        );
    }
}
