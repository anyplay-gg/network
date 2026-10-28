# Web API group `userProfile`

Status: **offered**, with body shapes that are guesses until a game we own has accepted them. The shape is established only by observing what a game we own does with an answer, never from anyone's documentation or code ([ADR 0021](../../docs/adr/0021-the-line-is-drawn-at-the-law.md)).

## Observed

| Game | Build | Run | Account |
|---|---|---|---|
| Minecraft Dungeons II | AnyPS5 with the bridge in observation mode ([anyplaynet/AnyPS5#1](https://github.com/anyplaynet/AnyPS5/pull/1)), relinked for Windows | about 130 s, ended by a GPU fault outside the network libraries | signed out: `sceNpGetAccountIdA` returned `0x80550006` |
| Overcooked! All You Can Eat | AnyPS5 with the bridge ([anyplaynet/AnyPS5#14](https://github.com/anyplaynet/AnyPS5/pull/14), [#15](https://github.com/anyplaynet/AnyPS5/pull/15)) | to the host screen, after [`communicationRestrictionStatus`](communicationRestrictionStatus.md) was answered | signed in to an AnyPlay cloud |

## Requests

### `GET /v1/users/{user}/friends`

| Part | Seen |
|---|---|
| `{user}` | signed out: `local_` and the game's local user ID in decimal (`local_268435456` for the first user, `0x10000000`); signed in: the signed-in account's ID in decimal |
| Query | signed out: empty, the path ends in `?`; signed in: `offset=0&limit=2000&filter=&order=` |
| Headers | one, `X-Psn-NpCppWebApi-Context` |
| Body | none |
| How often | signed out: 4 times, the first 105 s after the start, then every 5 to 10 s; signed in: once, when the player chooses to host, after `communicationRestrictionStatus` |
| Calls | `sceNpWebApi2CreateUserContext` once, then for each request `sceNpWebApi2CreateRequest`, `sceNpWebApi2AddHttpRequestHeader`, `sceNpWebApi2SendRequest` and `sceNpWebApi2DeleteRequest` |

### `GET /v1/users/{user}/blocks`

| Part | Seen |
|---|---|
| `{user}` | `me` |
| Query | `limit=2000` |
| Body | none |
| How often | once at the start of a signed-in run, next to `communicationRestrictionStatus`; while the call was not offered, `sceNpWebApi2SendRequest` returned `0x80553406` with status 0 |

### `GET /v1/users/profiles?accountIds={id}`

| Part | Seen |
|---|---|
| Query | `accountIds=` the signed-in account's ID |
| How often | 5 times in a row when hosting, after the friends request |

After these calls the game talks to its publisher's own online service over HTTPS. That is out of scope and stays unanswered ([ADR 0021](../../docs/adr/0021-the-line-is-drawn-at-the-law.md): no standing in for a publisher service that still runs).

## What the game did

- **"Not offered"** (`sceNpWebApi2SendRequest` returned `0x80553406`): Minecraft Dungeons II deleted the request and asked again 5 to 10 s later (the run ended for another reason before anything else could be seen). Overcooked! asked for the friends once and for the profiles five times.

## What the net service answers

Only for `GET`, and only while a player is signed in. Everything else (another method, another path of the group, signed out) stays "not offered". Nothing is read from the cloud per request: the net service already holds the friend list (read on going online, kept current by the cloud's `friends` push) and answers from it, however often a game polls.

### The id

The id a game sees for an account is the **AnyPlay account number in decimal**, for the player and for every friend. It is the same number the bridge reports as the signed-in user's account ID, the number in a session member's `account_id`, and the number a friend's PC reports for that friend. So a friend has the same id on both players' PCs, and the game can use it later for sessions and invitations. The default writes ids as strings (see the shape keys).

### `GET /v1/users/{user}/friends`

Offered when `{user}` is the signed-in account's ID or `me`. Another ID, and the `local_` form of a signed-out player, stay "not offered": the friends of another player are not ours to tell. The answer lists the player's **accepted AnyPlay friends** and no one else. Pending requests, blocked accounts and the player themself never appear.

- **Order:** by handle without regard to case, then by account number. The order is stable, so paging is consistent between calls.
- **Paging:** `offset` (default 0) skips that many friends; `limit` (default and maximum 2000) bounds the page. A missing, empty or non-numeric value means the default; `limit=0` gives an empty page; an `offset` past the end gives an empty page.
- **`filter` and `order`:** read and ignored (the game sends them empty).
- Default answer, status 200, `Content-Type: application/json`; `total` is the number of friends before paging:

```json
{ "friends": ["1002", "1005"], "offset": 0, "limit": 2000, "total": 2 }
```

### `GET /v1/users/{user}/blocks`

Offered under the same rules as `friends`: `{user}` is the signed-in account's ID or `me`, anything else stays "not offered". The answer lists the **AnyPlay accounts the player blocked** and no one else, taken from the `blocked` list of the same cloud friend list the friends come from, so nothing more is asked of the cloud. It never says who blocked the player, and it holds nothing about accounts of any platform. The player themself never appears. Order and paging are those of `friends` (`offset`, `limit`, default and maximum 2000, `total` before paging). A player with no blocks gets status 200 and an empty list. A blocked account is also left out of `friends` and `profiles`.

```json
{ "blocks": ["1007"], "offset": 0, "limit": 2000, "total": 1 }
```

### `GET /v1/users/profiles?accountIds={id}[,{id}...]`

`accountIds` may be repeated and may hold a comma-separated list (`%2C` works too); `me` stands for the player. The answer holds a profile for each requested account that is **the player or one of their accepted friends**, in the order asked, each once, at most 100 per request. Any other id is **left out**: an account that is not a friend, a blocked one, one that does not exist and a value that is not a number all look the same, so the answer never says whether an account exists. A request with nothing to show gets status 200 and an empty list.

A profile carries the account number and the AnyPlay **handle as the online name**, nothing else. The handle is what [ADR 0012](../../docs/adr/0012-private-by-default.md) lets any account see, so a friend who keeps their profile private looks like any other friend. Avatar, about text, trophies, playtime and presence are not part of the default answer: the avatar picture lives on the cloud and the game cannot fetch it, and the presence of someone who appears offline ([ADR 0033](../../docs/adr/0033-appearing-offline-hides-presence-from-everyone.md)) must not leak through a game. What a friend shows of the rest of their profile stays decided by their privacy settings in the launcher and on the web.

```json
{ "profiles": [ { "accountId": "1001", "onlineId": "me" }, { "accountId": "1002", "onlineId": "amy" } ] }
```

The answers carry no token and say nothing about a platform subscription.

### Log

Each answer is logged at debug level (`ANYPLAY_NET_LOG=anyplay_net::webapi=debug`) with the call, a short hash of the account ID, how many entries were returned, the status and whether an override was used. No handle and no id.

## Trying other shapes (testers)

Use the override file of the other groups: `ANYPLAY_NET_WEBAPI_OVERRIDES=<path>` (off when unset; for testing only). It is read on every request, so edit it while the game runs; a file that cannot be read or parsed is logged as a warning and the defaults are used. The group is `userProfile`, the three calls are `friends`, `blocks` and `profiles`. Each call takes a `shape` for the default answer, a whole reply, or both (the whole reply wins):

```json
{
  "userProfile": {
    "friends":  { "shape": { "items": "objects", "ids": "number", "friends_key": "list",
                             "id_key": "id", "name_key": "name", "limit_key": "",
                             "offset_key": "start", "total_key": "totalItemCount" } },
    "profiles": { "status": 200, "body_text": "{\"items\":{profiles}}" }
  }
}
```

| Key of `shape` | Default | Meaning |
|---|---|---|
| `ids` | `"string"` | `"string"` writes `"1002"`, `"number"` writes `1002` |
| `items` | `"ids"` | what an entry is in the `friends` and `blocks` lists: `"ids"` or `"objects"` (id and name, like a profile) |
| `friends_key`, `blocks_key`, `profiles_key` | `friends`, `blocks`, `profiles` | the array's name |
| `id_key`, `name_key` | `accountId`, `onlineId` | the fields of a profile, and of a friend written as an object |
| `offset_key`, `limit_key`, `total_key` | `offset`, `limit`, `total` | the paging fields of the `friends` and `blocks` answers |

A key set to `""` is left out. A whole reply has `status` (default 200), `headers` (default a JSON content type), `body` (any JSON value) and `body_text` (sent byte for byte, an empty string for an empty body; it wins over `body`). In `body_text` these are replaced by live values: `{account}`, `{handle}`, `{total}`, `{count}`, `{ids}` (the JSON array of the ids shown), `{profiles}` (the JSON array of the profiles shown), and for `friends` and `blocks` also `{offset}` and `{limit}`. They follow the `shape`. Write down in this file what the game did with each shape you tried.

## Unknown

- The body fields, status and headers the game accepts for each call, and whether it reads the body at all. The default shapes above are guesses.
- The header's value, and whether an answer has to carry it back.
- Whether the game wants presence, avatars or more profile parts, and whether it checks the profiles against the friend list.
- What it does with an id from the friend list when it starts a session or sends an invitation.

## Next

1. A run signed in to an AnyPlay cloud with a friend added, to see whether Overcooked! accepts the default shapes, and the override file for the shapes it does not.
2. Write down here what the game accepted.
