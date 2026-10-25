# Cloud API

The API between the net service and a cloud. Version 1 of the paths, still a draft: it changes without notice until the first public cloud runs.

The Rust reference is [`crates/api`](../crates/api). The cloud in [anyplay-gg/cloud](https://github.com/anyplay-gg/cloud) implements it. Nothing here knows the platform's formats; the net service translates those ([ADR 0001](../docs/adr/0001-bridge-forwards-net-service-translates.md)). The web portal in the player's browser uses a separate API on the browser's session cookie: [Portal API](portal-api.md).

## Conventions

- Every path starts with `/v1`. Bodies are JSON in UTF-8, at most 64 KiB.
- Times are Unix seconds. Account IDs are integers between 1 and 2^53 - 1, so every JSON parser reads them exactly.
- A request that needs an account carries `Authorization: Bearer <access token>`.
- Clients send `User-Agent: anyplay-net/<version>`.
- An error is an HTTP status with a body `{"error": "<code>", "message": "<English text>"}`. The codes:

| Code | Status | Meaning |
|---|---|---|
| `bad_request` | 400 | The body or a parameter is malformed. |
| `code_wrong` | 400 | The sign-in code does not match. |
| `code_expired` | 400 | The sign-in code or the sign-up is too old, or had too many tries. |
| `handle_invalid` | 400 | The handle breaks the rules below. |
| `unauthorized` | 401 | The access token is missing, expired or revoked. Refresh, then retry once. |
| `banned` | 403 | The account is banned. The body adds `until` (Unix seconds, absent for good) and `ban` with the statement of reasons and an appeal token (see Moderation). |
| `forbidden` | 403 | The account may not do this, for example because of a block. |
| `restricted` | 403 | The account may not do this yet or for now. The body adds `restriction` (`new_account` or `muted`) and `until` (Unix seconds; absent for a mute for good). |
| `not_found` | 404 | No such account, session or invitation, or one the caller may not see. |
| `handle_taken` | 409 | Another account has this handle. |
| `version_mismatch` | 409 | The game version or AnyPS5 build differs. The body adds `expected` with `version` and `build`. |
| `full` | 409 | The session has no free place. |
| `limit` | 409 | A per-account limit is reached (friends, requests, sessions). |
| `too_large` | 413 | The body or the session properties are too large. |
| `rate_limited` | 429 | Too many requests. `Retry-After` says how many seconds to wait. |
| `internal` | 500 | The cloud failed. |

## Objects

```text
Account   { id, handle }
Me        { id, handle, email, created }
Tokens    { access, access_expires, refresh, device }
Device    { name, platform, key }                  platform: "windows" | "linux" | "macos" | "other"; key: the device's public key or absent
Presence  { online, activity, revision }           activity: Activity or null
Activity  { title_id, title, version, session }    session: SessionRef or null
SessionRef{ id, joinable, members, max_members }
Friend    { account, since, presence }
Request   { account, since }
Session   { id, title_id, title, version, build, leader, max_members, visibility, properties, members, created, revision }
Member    { account, endpoint, relay, address, joined }
Invitation{ id, from, session, title_id, title, version, expires }
Relay     { url, token, expires }
Trophy    { number, grade, earned }                grade: "bronze" | "silver" | "gold" | "platinum"
TitleTrophies { title_id, trophies: [Trophy] }
Count     { bronze, silver, gold, platinum }
Card      { account, trophies, avatar }            trophies: Count, or null when hidden; avatar: hash, or absent
Privacy   { trophies, about, favourites, avatar, playtime }   each "friends" | "everyone" | "nobody"
Profile   { account, about, favourites, avatar, playtime }    each part null when unset or hidden from the caller
PlaytimeEntry { title_id, day, minutes }           day: "YYYY-MM-DD" (UTC)
TitlePlaytime { title_id, minutes }
Party     { id, leader, members, max_members, conversation, created, revision }
PartyMember { account, joined, endpoint, relay, voice }   endpoint: "" until announced; relay: URL or null; voice: bool
PartyInvitation { id, from, party, members, expires }
Conversation { id, kind, name, members, last, read, marks, updated, request, party }   kind: "direct" | "group" | "party"
ReadMark  { account, seq }
Message   { conversation, seq, sender, text, created }
```

- A **handle** has 3 to 16 characters from `A-Z a-z 0-9 _ -` and starts with a letter. Handles are unique without regard to case; the spelling of the owner is kept. Handles that look alike are one handle: the **skeleton** of a handle (`handle_skeleton` in `anyplay-api`) is the handle in lower case without `_` and `-`, with `rn` read as `m`, `vv` as `w`, `1` and `i` as `l`, and `0` as `o`, and an account can take a handle only when no other account has its skeleton (so `Flokzy`, `FIokzy`, `F1okzy` and `Flok_zy` are one). A skeleton that is taken answers `handle_taken`, like a taken handle. Some handles are reserved (names of the network, of staff roles, and of platforms), and reserved names are matched by skeleton too.
- `visibility` is `friends` (friends of a member may join), `invite` (only invited accounts) or `closed`.
- `properties` is opaque data for the game, as standard base64, at most 8 KiB once decoded.
- `endpoint` is the net service's tunnel key (an iroh endpoint ID) as lowercase hex, and `relay` the URL of the relay it is reachable through, or null. Other members dial it with both; the cloud publishes neither anywhere else.
- `address` is the member's virtual address in this session (see Events).
- `build` names the AnyPS5 build. Two players may only play together when `title_id`, `version` and `build` all match.
- `revision` rises with every change to a session, and with every change of an account's presence its friends are told about. Several nodes of one cloud may send news, so a copy can arrive after a newer one: a client keeps the highest revision it has seen and drops a copy with a lower one. A cloud that does not count sends 0, which a client always takes.

## Signing in

Accounts have no password. A device signs in with a code sent to the account's e-mail address, with a passkey, or in the player's browser, and then keeps a refresh token bound to a key of its own ([ADR 0005](../docs/adr/0005-sign-in-by-e-mail-code.md), [ADR 0016](../docs/adr/0016-accounts-are-managed-on-the-web.md)). The browser flow is the way the launcher signs in; the e-mail requests below stay for clouds without an account web and for older launchers.

| Request | Body | Answer |
|---|---|---|
| `POST /v1/auth/email` | `{email}` | `{challenge, expires, code_length}` |
| `POST /v1/auth/code` | `{challenge, code, device}` | `{result: "signed_in", tokens, me}` or `{result: "new_account", signup, expires}` |
| `POST /v1/auth/signup` | `{signup, handle, age_confirmed, device}` | `{tokens, me}` |
| `POST /v1/auth/refresh` | `{refresh, device_key}` and a proof | `Tokens` |
| `POST /v1/auth/signout` | none (bearer) | 204 |
| `POST /v1/auth/browser/token` | `{code, verifier, device}` | `{tokens, me}` |
| `POST /v1/auth/browser/link` | none (bearer) and a proof | `{url, expires}` |

- `auth/email` answers the same whether or not an account exists, so it cannot be used to test addresses. A code is 6 digits, lives 10 minutes and allows 5 tries.
- `age_confirmed` must be `true`: the player confirms being old enough to agree alone (16 in the EU unless a country sets a lower age). Younger players need a parent's consent, which this version does not collect.
- `device` carries `key`, the device's public key (below). A cloud may refuse a device without one (`bad_request`): see Devices without a key.
- An access token lives 15 minutes. A refresh token is replaced on every use. Using a replaced refresh token again signs the device out and so ends the whole chain of its refresh tokens, whether or not the request has a proof: only a copy can present it twice.
- `device_key` in a refresh request is for a device that has none yet; see Devices without a key. For a device that has a key it is ignored.

## Tokens

An access token is `v2.<payload>.<signature>`. The payload is JSON in unpadded URL-safe base64 with the fields `k` (`access` or `relay`), `aud` (`cloud` for access, `relay` for relay tokens), `kid` (the first 4 bytes of the SHA-256 of the signing public key, in lowercase hex), `a` (account), `d` (device, access tokens) or `e` (endpoint, relay tokens), `i` (issued, Unix seconds) and `x` (expires). The signature is Ed25519 over the bytes `anyplay-ticket-v2` + `0x00` + `v2.` + the payload part. A verifier refuses a wrong signature, a `kid` it does not have, a kind or audience that does not fit, and a token at or past `x`. Tokens of version 1 (`v1.<payload>.<signature>`, signature over `v1.` + payload, no `i`, `aud` or `kid`) are still read and have `i` 0, and are no longer made.

Test vectors, from the signing secret of 32 bytes of `0x07` (`kid` `fe812c12`):

```text
access, account 41, device 3, issued 1800000000, expires 1800000900:
v2.eyJrIjoiYWNjZXNzIiwiYXVkIjoiY2xvdWQiLCJraWQiOiJmZTgxMmMxMiIsImEiOjQxLCJkIjozLCJpIjoxODAwMDAwMDAwLCJ4IjoxODAwMDAwOTAwfQ.v5hqHZXmSEZZ7e4zoEjBo16I88VTCue4tXoeyZHnSfpieSu1We51qnpsZ5LQnXDg-6kc5QCT5-GAVjkVRRxCCg
relay, account 41, endpoint "ab" x 32, issued 1800000000, expires 1800086400:
v2.eyJrIjoicmVsYXkiLCJhdWQiOiJyZWxheSIsImtpZCI6ImZlODEyYzEyIiwiYSI6NDEsImUiOiJhYmFiYWJhYmFiYWJhYmFiYWJhYmFiYWJhYmFiYWJhYmFiYWJhYmFiYWJhYmFiYWJhYmFiYWJhYmFiYWJhYmFiIiwiaSI6MTgwMDAwMDAwMCwieCI6MTgwMDA4NjQwMH0.3mLYH9L8CmrFxXt8P6n2fwcCXLAev3vaP-uVdWvLDOjVjqwqKU_i84TOSNZ0fZF0Vox-JCT9xSfzyI-tumsoDw
```

An access token is accepted only while its device is signed in, its account is not banned and the token was issued at or after the account's "not before" time, which "sign out everywhere" sets to now. Relay tokens are checked by relays alone and stay valid until they expire.

## Device keys and proofs

At sign-in a device sends `key`: an Ed25519 public key, 32 bytes in unpadded URL-safe base64. The cloud keeps it with the device. The private key stays on the PC, in the entry that holds the refresh token.

A **proof** is the header `AnyPlay-Proof: v1.<time>.<nonce>.<signature>`: `time` is Unix seconds in decimal, `nonce` 16 random bytes in unpadded URL-safe base64, and the signature is Ed25519 over the UTF-8 text

```text
anyplay-proof-v1\n<METHOD>\n<path>\n<time>\n<nonce>\n<BASE64URL(SHA-256(token))>
```

where `path` has no query and `token` is the text of the refresh token (for refresh) or of the access token (for the link). The cloud accepts a proof whose time is within 60 seconds of its own clock, once: it keeps the nonce until the time window ends, in the store, so every node refuses a replay. A proof of the wrong time answers `bad_request` (the player's clock is off) and leaves the device signed in; any other failure answers `unauthorized`, also without signing the device out.

Test vector: key secret of 32 bytes of `0x07` (public key `6kpsY-KcUgq-9VB7Ey7F-ZVHdq6-vnuSQh7qaRRG0iw`), `POST`, `/v1/auth/refresh`, token `refresh-token-1`, time 1800000000, nonce 16 bytes of `0x01`:

```text
v1.1800000000.AQEBAQEBAQEBAQEBAQEBAQ.nyoO-XK2j-igBkE6cd81u4D3dW5O4JKKa62lLTRZU4LOfJj6Vt8Gopf6TGpJBqneapSZxWgs9x7C26rXSQxuAQ
```

Which requests carry a proof: **refresh**, for every device that has a key, and **`browser/link`**. Other requests carry the access token alone: it lives 15 minutes and its device must exist.

### Devices without a key

A device signed in before keys existed is a *legacy device*. It may refresh without a proof until the end of its grace period (30 days after the cloud was upgraded; `legacy_device_days` for devices that sign in without a key later). A refresh with `device_key` and a proof made by that key binds the device; from then on it is a normal device, and a device is never unbound. After the grace period, or on a cloud that sets `legacy_device_days` to 0, a legacy device gets `unauthorized` and must sign in again.

## Browser sign-in and the account web

A cloud that has a public address serves the account web ([ADR 0016](../docs/adr/0016-accounts-are-managed-on-the-web.md)): pages to sign in, create an account and manage it. `GET /v1/info` names its origin in `web`. The pages are not part of this API; what the net service uses is:

1. `GET <web>/web/authorize?client=net&intent=sign_in|create&redirect_uri=<uri>&state=<state>&code_challenge=<challenge>&code_challenge_method=S256&device=<name>` in the system browser. `redirect_uri` must be `http://127.0.0.1:<port>/<path>` or `http://[::1]:<port>/<path>` (port 1024 or higher, no user, query or fragment); `state` is 16 to 128 characters of `A-Z a-z 0-9 - _`; `code_challenge` is `BASE64URL(SHA-256(verifier))` of a verifier of 43 to 128 characters (RFC 7636). Anything else is refused with an error page and no session.
2. The player signs in or creates the account in the browser. The cloud then sends the browser to `<redirect_uri>?code=<code>&state=<state>`. The code lives 60 seconds and can be used once.
3. `POST /v1/auth/browser/token` with `{code, verifier, device}` answers `{tokens, me}` for a new device bound to `device.key`. A code that is unknown, used or expired answers `code_expired`; a verifier that does not match answers `code_wrong`, and the code is gone either way.
4. `POST /v1/auth/browser/link`, with the bearer token and a proof, answers `{url, expires}`: a link (60 seconds, single use) that opens the account web signed in as the account. The page behind it asks before it signs in. It is `<web>/web/link?c=<code>`, or `<web>/link?c=<code>` when another service serves the account web ([Portal API](portal-api.md#the-link-of-the-account-button)).

The browser's own session on these pages is a cookie that never reaches the API.

## Accounts

| Request | Answer |
|---|---|
| `GET /v1/me` | `Me` |
| `DELETE /v1/me` | 200 `{deletion: {at}}`. Schedules the deletion of the account for `at` (Unix seconds), 30 days from now, signs out every device and every browser, and refuses the access tokens issued so far; the player can cancel on the [web portal](portal-api.md#deleting-the-account) until `at`. A request that comes again keeps the first `at`. At `at` the cloud deletes the account with its devices, friends, requests, blocks, trophy records, profile, avatar and playtime. Reports the account filed lose their link to it; reports about it, the actions against it and the ban on its e-mail address stay until their retention ends (see Moderation). |
| `GET /v1/me/export` | Everything the cloud stores about the account, as JSON, trophy records, privacy settings, profile, avatar and playtime included. |
| `GET /v1/accounts/{id}` | `Account` |
| `GET /v1/accounts?handle=<handle>` | `Account`; the lookup is exact apart from case. There is no search. |

## Friends and blocks

| Request | Body | Answer |
|---|---|---|
| `GET /v1/friends` | | `{friends: [Friend], incoming: [Request], outgoing: [Request], blocked: [Account]}` |
| `POST /v1/friends/requests` | `{handle}` | `{state: "requested" \| "friends"}` |
| `POST /v1/friends/requests/{id}/accept` | | 204 |
| `DELETE /v1/friends/requests/{id}` | | 204: declines an incoming or withdraws an outgoing request |
| `DELETE /v1/friends/{id}` | | 204 |
| `PUT /v1/blocks/{id}` | | 204: also ends a friendship, requests and invitations between the two |
| `DELETE /v1/blocks/{id}` | | 204 |

- A request to an account that already asked us makes both friends at once (`state: "friends"`).
- A request to an account that blocked us answers `requested` and does nothing, so a block stays private.
- Limits: 500 friends, 100 outgoing requests.
- A new account (younger than the cloud's `new_account_hours`, 24 by default) may send friend requests, but at most 10 during that time (`new_account_friend_requests_per_day`): the 11th answers `restricted` with `restriction: "new_account"` and `until` set to the moment the account is old enough. Accepting a waiting request does not count. A muted account sends none: `restricted` with `restriction: "muted"`.

## Presence

`PUT /v1/presence` with `{activity}` (an `Activity` without `session`, or null) sets what friends see. The cloud adds `session` itself: the session the account is in for that title, when friends may join it. An account is online while its net service holds the event connection.

## Sessions

| Request | Body | Answer |
|---|---|---|
| `POST /v1/sessions` | `{title_id, title, version, build, max_members, visibility, properties, endpoint, relay}` | `Session` |
| `GET /v1/sessions/{id}` | | `Session` |
| `POST /v1/sessions/{id}/join` | `{version, build, endpoint, relay, invitation}` | `Session` |
| `POST /v1/sessions/{id}/leave` | | 204 |
| `PATCH /v1/sessions/{id}` | any of `{properties, visibility, max_members, leader}` | `Session` (leader only) |
| `DELETE /v1/sessions/{id}/members/{account}` | | 204 (leader only) |

- `max_members` is 2 to 64. An account is in at most 4 sessions at a time.
- Joining checks, in this order: blocks between the joiner and any member (`forbidden`), visibility (`not_found`), the version guard (`version_mismatch`) and capacity (`full`). `invitation` is the ID of an invitation, or null. A member joining again only updates its `endpoint` and `relay`, for example after its net service moved to another relay.
- When the leader leaves, the longest-standing member leads. The last member to leave closes the session.
- Sessions live in the cloud's store, so they outlast a restart of the cloud. A member whose net service stays disconnected for 30 seconds leaves.

## Invitations

| Request | Body | Answer |
|---|---|---|
| `POST /v1/sessions/{id}/invitations` | `{to: [id]}` | `{sent: [id]}`: only members invite, only friends receive |
| `DELETE /v1/invitations/{id}` | | 204: declines |

An invitation lives 15 minutes, or until its session closes.

## Relays

`POST /v1/relays/tickets` with `{endpoint}` answers `{relays: [Relay]}`. Each `token` lets this endpoint, and only it, use that relay until `expires` (24 hours). The net service asks again before then. Relays check the token themselves; they never call the cloud ([ADR 0007](../docs/adr/0007-relays-check-signed-tickets.md)).

The token names the account it was issued to. A relay holds the account to a number of connections at once, refuses a token that is valid for much longer than 24 hours, and limits what each client sends; the numbers are the relay operator's settings, not part of this specification. The cloud limits how often an account may ask for tickets (see Limits), and answers `rate_limited` with `Retry-After` when it is over.

## Trophies and privacy

The cloud keeps a **trophy record** per trophy a player earned: the title, the trophy's number in the game, its grade and when ([ADR 0009](../docs/adr/0009-trophy-records-not-trophy-texts.md)). Names, descriptions and pictures are not part of it; each launcher shows them from its own copy of the game. Trophies are private by default: friends see them, nobody else ([ADR 0012](../docs/adr/0012-private-by-default.md)).

| Request | Body | Answer |
|---|---|---|
| `PUT /v1/me/trophies/{title_id}` | `{trophies: [Trophy]}` | `{added}`: how many records are new |
| `GET /v1/accounts/{id}/trophies` | | `{titles: [TitleTrophies]}`, or `not_found` when the account's trophies are hidden from the caller |
| `GET /v1/titles/{title_id}/rarity` | | `{title_id, players, trophies: [{number, earned_by}]}` |
| `GET /v1/cards?accounts=1,2,3` | | `{cards: [Card]}` for the accounts the caller may see, at most 100 |
| `GET /v1/me/privacy` | | `Privacy` |
| `PUT /v1/me/privacy` | `Privacy` | `Privacy`; a part left out takes the default, `friends` |

- `number` is 0 to 2047 and `earned` is in Unix seconds. One upload carries at most 512 trophies. A trophy recorded already keeps its first `earned` and grade.
- Who sees an account's trophies: the account itself always; its friends unless `trophies` is `nobody`; everyone else only with `everyone`. A block in either direction hides them.
- `players` counts the accounts with at least one record for the title. Rarity is only given once 10 players have the title; below that `players` is 0 and `trophies` empty, so it cannot tell which trophies one person earned.
- A gamer card's `trophies` counts the account's records by grade, or is null when its trophies are hidden from the caller. Its `avatar` is the hash of the account's avatar, absent when there is none or the caller may not see it. Nothing else is on a card.

## Run reports

A **run report** is a player's answer to "How did it run?" after a session: the title, the game version, the toolkit version, a verdict and the player's system. Nothing else leaves the player's machine: no paths, hardware IDs or game data ([ADR 0021](../docs/adr/0021-the-line-is-drawn-at-the-law.md) draws the line; a run report is an opinion, not a game file).

| Request | Body | Answer |
|---|---|---|
| `PUT /v1/me/run-reports/{title_id}` | `{version, toolkit, verdict, os}` | `204` |
| `GET /v1/titles/{title_id}/runs?version=` | | `{title_id, version, reports, verdict, counts}` |

- `verdict` is `no_start`, `menus`, `in_game` or `playable`. `os` is `windows`, `linux` or `macos`. `version` and `toolkit` are 1 to 32 characters from `A-Z a-z 0-9 . _ + -` (`bad_request` otherwise).
- An account has one report per title, game version and toolkit version; a new `PUT` replaces the old one, and the cloud keeps the time of the last change. Deleting an account deletes its run reports; the export lists them.
- `GET` counts accounts, not rows: an account counts once, with its latest report in the scope asked for, so one player cannot make a summary alone. With `version`, it counts the reports for that game version when there are at least 3, and else the reports for all versions. `version` in the answer is the game version the counts are about, or null for all versions. `counts` is `{no_start, menus, in_game, playable}`; `reports` is their sum (the number of accounts) and `verdict` the most reported one (on a tie the lower one, so a summary promises no more than players found).
- Below 3 reports `reports` is 0, `verdict` is null and all counts are 0, so a summary cannot point at one player. A website may list the same summaries publicly later; they hold no account.
- `PUT` is limited to 30 per account per hour (`rate_limited`).

## Profiles, avatars and playtime

A profile has a short text, up to 3 favourite titles, an avatar and the playtime per title. Each part has its own visibility in `Privacy` (`about`, `favourites`, `avatar`, `playtime`), and all start at `friends`. The rule is the one of the trophies: the account itself always; its friends unless the part is `nobody`; everyone else only with `everyone`; a block in either direction hides it.

| Request | Body | Answer |
|---|---|---|
| `GET /v1/accounts/{id}/profile` | | `Profile` with the parts the caller may see; the others are null |
| `PATCH /v1/me/profile` | `{about?, favourites?}` | the caller's `Profile`. A part that is left out stays; `""` or `[]` clears it |
| `PUT /v1/me/avatar` | the picture, `Content-Type: image/png` or `image/jpeg` | `{avatar}`: the new hash |
| `DELETE /v1/me/avatar` | | 204 |
| `GET /v1/avatars/{hash}` | | the picture, `image/png`, no token needed |
| `PUT /v1/me/playtime` | `{entries: [PlaytimeEntry]}` | `{changed}`: how many entries were new or raised their day |

- `about` has at most 160 characters of plain text: no control characters and no links (a scheme such as `https:`, `www.`, or a word such as `example.com`). Surrounding whitespace is cut. `favourites` has at most 3 distinct title IDs.
- A part is null both when the account has not set it and when the caller may not see it, so a null tells a stranger nothing.
- **Avatars.** The body is at most 1 MB (the one exception to the 64 KiB body limit) and must really be a PNG or JPEG, whatever it claims; a picture over 4,096 pixels on a side or that needs more than 64 MiB to decode is refused. The cloud decodes it, turns it upright by its EXIF orientation, crops it to a centred square, scales it to 256 by 256 pixels and encodes it again as PNG, so no metadata of the original survives. It is stored under the SHA-256 of the result (64 lowercase hex digits), which is the `avatar` in profiles and cards. The picture at `/v1/avatars/{hash}` never changes, so it is served with `Cache-Control: public, max-age=31536000, immutable` and an `ETag`. The hash is the only address; it is given out only to those who may see the avatar. A new avatar replaces the old one, which stops existing. Avatars are the player's own pictures, never taken from games.
- **Playtime.** `day` is a real UTC date from 2000-01-01 to tomorrow, `minutes` is 0 to 1,440, one upload carries at most 512 entries, and an account keeps at most 20,000 days of titles. For each title and day the cloud keeps the highest `minutes` it has seen, so an upload can be sent again and never adds twice. `Profile.playtime` lists the total minutes per title, most played first.

## Reports

`POST /v1/reports` with `{account, reason, details, conversation, seq}` answers 204. `reason` is `cheating`, `harassment`, `spam`, `name` or `other`; `details` has at most 1,000 characters. `conversation` and `seq` are optional and go together: they name one message written by `account` in a conversation the reporter can read. The reporter cannot report a message they cannot see (`not_found`), or one written by someone else (`bad_request`).

- The cloud saves a snapshot of what was reported with the report: the reported handle, and for a message its text, so a report is evidence even when the message was swept or the account is gone. A moderator reads it as `content`.
- An account may have one open report about the same account at a time (`limit`), and files at most 20 reports a day and 5 in 10 minutes (`limit`, `rate_limited`). The numbers are settings of the cloud.

## Moderation

Reports go into a queue. A **moderator** reads the queue and takes an **action** with a reason; every action is written to an append-only **audit log**; the player concerned reads a **statement of reasons** and may **appeal** ([ADR 0015](../docs/adr/0015-moderators-are-accounts.md)).

### Who may moderate

A moderator is an account with the moderator role, which only the operator grants. A moderator calls the `/v1/moderation/...` paths below with their normal access token. The operator can call the same paths with the admin token as the bearer token: that is how the first moderator is named, and the operator acts under the name `operator`. A request without the role answers `forbidden`. A moderator may not act on themselves, and only the operator may act on another moderator.

### The queue

| Request | Body | Answer |
|---|---|---|
| `GET /v1/moderation/reports?status=open&limit=100` | | `[ReportView]`, oldest first when `status=open`, newest first otherwise |
| `GET /v1/moderation/reports/{id}` | | `ReportView` |
| `POST /v1/moderation/reports/{id}/dismiss` | `{reason}` | `ReportView`: no action |
| `POST /v1/moderation/actions` | `ActionRequest` | `ActionView` |
| `POST /v1/moderation/actions/{id}/lift` | `{reason}` | `ActionView`: takes the action back |
| `GET /v1/moderation/actions?account={id}` | | `[ActionView]` about that account, newest first |
| `GET /v1/moderation/appeals?status=open` | | `[AppealView]`, oldest first |
| `POST /v1/moderation/appeals/{id}/decision` | `{outcome: "uphold" \| "reverse", reason}` | `AppealView` |
| `GET /v1/moderation/log?before={id}&limit=100` | | `[AuditEntry]`, newest first |
| `GET /v1/moderation/moderators` | | `[Moderator]` |
| `POST /v1/moderation/moderators` | `{handle}` | `Moderator` (operator only) |
| `DELETE /v1/moderation/moderators/{id}` | | 204 (operator only) |

```text
ReportView  { id, reporter, reported, reported_deleted, reason, details, content, created,
              status, outcome, decided, decided_by, action }
              status: "open" | "decided"; outcome: "dismissed" | "actioned"; reporter absent once deleted
ActionRequest { account, kind, rule, reason, days, report }
ActionView  { account, actor, statement }
AppealView  { id, action, created, decided_by }      action: ActionView, whose statement holds the appeal
Statement   { id, kind, rule, reason, created, until, lifted, from_report, automated, appeal }
AppealInfo  { status, deadline, text, answer, decided }
Actor       { id, handle }                           id absent for the operator
```

### Actions

| `kind` | Effect | `days` |
|---|---|---|
| `warning` | A recorded notice. Nothing is restricted. | not allowed |
| `mute` | No friend requests and no messages, until it ends. The account stays signed in. | required, 1 to 3650 |
| `ban` | The account is signed out on every node and cannot sign in again; its e-mail address cannot sign up. | absent: for good |

- `rule` is one of `cheating`, `harassment`, `spam`, `name`, `illegal`, `other`: the rule of the terms the action rests on. `reason` has 1 to 500 characters and is what the player reads, so it is written for them. It must not name the reporter.
- With `report`, the action decides that report. A report is decided once; deciding it again answers `limit`. Other open reports about the account stay open for a moderator to dismiss.
- A newer ban replaces an older one on the same account. A lifted action stays in the list with `lifted`.
- `removal` of content (SOCIAL "Moderation") is not an action yet, because nothing a player writes is stored for others to read. It joins `kind` with the first such content.

### What the player sees

An action against a player produces a **statement of reasons**: what was done (`kind`, `until`), why (`reason`, `rule`), whether a report started it (`from_report`), whether software decided alone (`automated`, always `false` for now), and how to appeal (`appeal`). It names neither the reporter nor the moderator.

- A warning or a mute arrives as the event `action` on the player's connection. A ban arrives as `banned` with `statement`, then the connection closes (code 4003).
- `GET /v1/me/actions` answers `{actions: [Statement]}`, newest first, for every action the cloud still keeps.
- A banned account cannot call the rest of the API, so the `banned` error of a sign-in or a refresh carries `ban: {statement, appeal_token}`. At sign-in the statement is only revealed after the e-mail code is entered, never to someone who merely knows the address. `appeal_token` is a bearer token for seven days. With it, or with the access token of a device that is still signed in, a banned person may call the two paths under `/v1/me/actions`.

### Appeals

`POST /v1/me/actions/{id}/appeal` with `{text}` (1 to 1,000 characters) answers the `Statement`, now with `appeal.status` `open`.

- One appeal per action, free of charge, until `appeal.deadline`: six months after the action by default. A second one answers `limit`; one after the deadline answers `forbidden`.
- A moderator decides it with `uphold` or `reverse` and a reason. When possible it is not the moderator who took the action: the cloud refuses the same moderator (or the operator, for an action of the operator) as long as another moderator exists (`forbidden`). Reversing lifts the action. The player gets the event `action` and reads `appeal.answer`.
- Appeals are decided by people. The cloud never decides one by itself.

### The audit log

Every action, dismissal, lifting, appeal and decision, and every grant or withdrawal of the role, adds a line: time, actor, what, the account concerned with the handle it had then, the reason, the rule, and the report, action or appeal it belongs to. Nothing in the API changes or removes a line; the cloud deletes lines only when its retention period for the log ends.

### Retention

Reports, actions and appeals are kept while they matter and then deleted: an expired or lifted action and a decided report six months after the end or decision, an open report after a year, the log after three years. The numbers are settings of the cloud. A ban for good is kept until a moderator lifts it.

## Parties

A **party** is up to 8 friends who stay together across games, with one chat. It belongs to no game, and an account is in at most one party. The leader invites; members leave when they like. The party's `revision` rises with every change, as a session's does.

| Request | Body | Answer |
|---|---|---|
| `GET /v1/party` | | `Party`, or `not_found` when the caller is in none |
| `POST /v1/party` | | `Party`: a party of one, the caller leading it. `limit` when the caller is in one already. |
| `POST /v1/party/invitations` | `{to: [id]}` | `{sent: [id]}`: leader only; starts a party when the caller is in none |
| `GET /v1/party/invitations` | | `{invitations: [PartyInvitation]}`: the ones the caller holds |
| `POST /v1/party/invitations/{id}/accept` | | `Party` |
| `DELETE /v1/party/invitations/{id}` | | 204: declines |
| `POST /v1/party/leave` | | 204 |
| `DELETE /v1/party/members/{account}` | | 204 (leader only) |
| `PATCH /v1/party` | `{leader}` | `Party` (leader only; the new leader is a member) |
| `PUT /v1/party/endpoint` | `{endpoint, relay, voice}` | `Party`: tells the party where the caller's net service is |
| `POST /v1/party/session` | `{session}` | `{sent: [id]}` (leader only) |

- Only friends of the leader receive an invitation, and not those who are in the party, who blocked a member or whom a member blocked. An invitation lives 15 minutes, or until its recipient joins any party.
- Accepting answers `limit` when the caller is in a party already, `full` when the party has 8 members, and `forbidden` when a block lies between the caller and a member.
- When the leader leaves, the longest-standing member leads. The last member to leave ends the party, its invitations and its chat. A member whose net service stays disconnected for 15 minutes leaves.
- `POST /v1/party/session` moves the party into one of the leader's sessions: every other member who is not in the session gets an ordinary session invitation (see Invitations), which lets them in even when the session is for invited players only. The leader must be a member of the session, and the session must not be closed. Nothing else about the party changes; it goes on after the game.
- `PUT /v1/party/endpoint` is how the members of a party find each other for voice ([voice](voice.md)). `endpoint` and `relay` are the caller's tunnel key and relay, as for a session `Member`, and `voice` says that the caller's net service takes part in the party's voice and accepts voice connections from the other members. The net service sends it when it joins a party, when its relay changes and when `voice` changes, and again after every reconnect. The cloud stores the three on the caller's `PartyMember`, raises the party's `revision` only when one of them changed, and answers `not_found` when the caller is in no party and `bad_request` for an `endpoint` that is not 64 lowercase hex characters or a `relay` that is not an `https` URL. Two members of one party may not name the same `endpoint`: the second request is answered `limit`. A member who has announced nothing has `endpoint` `""`, `relay` null and `voice` false. Only members of the party ever receive `endpoint` and `relay`: the invitation (`PartyInvitation`) carries neither. A net service dials only members whose `voice` is true, and accepts voice connections from every member the party lists.
- The party's chat is a conversation of kind `party` (`Party.conversation`). A member sees what is written after it joined.

## Shares

Share play between two party members is drafted in the [share play spec](share-play.md#cloud-api): the `Share` object, `/v1/shares` and `/v1/share`, and the events `share_invitation`, `share` and `share_gone`. Not implemented.

## Conversations and messages

A **conversation** is `direct` (two accounts, one per pair), `group` (2 to 32 accounts) or `party` (see above). Messages are text of 1 to 2,000 characters. They are stored so that every device of the receiver gets them, and the cloud deletes them after 30 days, or the time the operator set ([ADR 0011](../docs/adr/0011-messages-are-stored-to-be-delivered.md)).

Every message has a number `seq` that counts up by one in its conversation, starting at 1. Numbers are given in the order the messages were stored, and a message with a lower number is always stored before one with a higher number ([ADR 0013](../docs/adr/0013-messages-are-numbered-per-conversation.md)).

| Request | Body | Answer |
|---|---|---|
| `GET /v1/conversations` | | `{conversations: [Conversation], requests: [Conversation]}`, newest first |
| `GET /v1/conversations/{id}` | | `Conversation` |
| `POST /v1/conversations` | `{accounts: [id], name}` | `Conversation`: with one account, the direct conversation (made when there is none); with 2 to 31, a new group |
| `POST /v1/conversations/{id}/members` | `{accounts: [id]}` | `{added: [id]}`: groups only |
| `DELETE /v1/conversations/{id}` | | 204: leaves a group, or declines a message request |
| `POST /v1/conversations/{id}/accept` | | `Conversation`: accepts a message request |
| `POST /v1/conversations/{id}/messages` | `{text}` | `Message` |
| `GET /v1/conversations/{id}/messages?after=N` | | `{messages: [Message], last}`: the next messages after number `N`, oldest first |
| `GET /v1/conversations/{id}/messages?before=N` | | the newest messages below `N`, oldest first |
| `GET /v1/conversations/{id}/messages` | | the newest messages |
| `PUT /v1/conversations/{id}/read` | `{seq}` | 204 |

- A fetch answers at most `limit` messages (default 50, at most 100). `last` is the number of the conversation's newest message. A conversation that the caller is not in answers `not_found`.
- `Conversation.last` is the newest number, `read` the number the caller read up to, `marks` how far the others read, `request` whether the caller still has to accept it. A client keeps the highest number it has fetched for each conversation. After a break it reads the list, and for each conversation whose `last` is above its own number it fetches `after` its own number until it has reached `last`. Messages a block kept away are missing from a fetch, so numbers can skip.
- **Groups.** The creator names friends only (`forbidden` otherwise, also for a block in either direction). Any member may add its own friends; those who are members already, who are no friends of the adder, whom a member blocked or who blocked a member are left out of `added`. A new member sees messages written from then on. An account is in at most 50 groups. A group is `full` at 32.
- **Message requests.** Opening a conversation with an account that is no friend makes a request for that account. It is listed in `requests` once it holds a message, and the receiver can read it; links in it stay inactive in the launcher, and nothing tells the sender that it was read. The receiver is told once (`message_request`). The receiver accepts it, declines it, or answers, which accepts it. A decline is silent: the sender can write on, but nothing reaches the receiver. A stranger may send 5 messages before the receiver answers (`limit`) and may open at most 10 conversations with strangers an hour (`rate_limited`); an account holds at most 100 requests. An account younger than the cloud's `new_account_hours` (24 by default) may not open a conversation with a stranger (`restricted`, `restriction: "new_account"`, `until`). It may write to its friends at once. A muted account may not send messages at all (`restricted`, `restriction: "muted"`). Once the two are friends, a request is an ordinary conversation.
- **Blocks.** If the caller blocked the other account of a direct conversation, writing and opening it answer `forbidden`. If the other blocked the caller, writing succeeds and nothing is delivered. In a group, a block hides one sender's messages from one member, in both directions. A message a block kept away stays away after the block is lifted.
- **Read marks.** `PUT .../read` moves the caller's mark forward to `seq` (at most `last`); a lower value changes nothing. Marks are shown to the members who accepted the conversation; the marks of a request stay with the receiver's own devices. Writing reads up to the message written.
- A conversation with a friend cannot be left. A party's chat ends with the party.
- **Limits.** 30 messages per account per minute (`rate_limited`). Deleting an account deletes its messages and its direct conversations; the export lists the messages the account wrote and its conversations.

## Events

`GET /v1/events` upgrades to a WebSocket. The upgrade carries the bearer token. The cloud sends one JSON object per text message, each with a `type`:

| Type | Fields | Meaning |
|---|---|---|
| `welcome` | `address, server_time` | Sent first, once the cloud pushes to this connection. A client counts as online from here and fetches its lists after it, so no change falls between the two. `address` is the account's preferred virtual address (see Virtual addresses). |
| `friends` | | The friend lists changed; fetch `GET /v1/friends`. |
| `presence` | `account, presence` | A friend's presence changed. |
| `invitation` | `invitation` | A friend invited this account. |
| `invitation_gone` | `id` | An invitation expired, was declined or its session closed. |
| `session` | `session` | A session this account is in changed (members, leader, properties). |
| `session_gone` | `id` | This account is no longer in the session: it closed, or the leader removed it. |
| `signed_out` | | This device was signed out or the account deleted. Forget the tokens. |
| `banned` | `until, statement` | The account was banned; `statement` is the statement of reasons when the cloud has one. The connection closes next. |
| `action` | `statement` | A moderator warned or muted this account, or decided an appeal. |
| `party` | `party` | The party this account is in changed. Keep the highest `revision` and drop an older copy. |
| `party_gone` | `id` | This account is no longer in the party. |
| `party_invitation` | `invitation` | The leader of a party invited this account. |
| `party_invitation_gone` | `id` | A party invitation expired, was declined or used up. |
| `message` | `conversation, seq` | A new message. Fetch the messages after the number you hold. It carries no text. |
| `message_request` | `conversation` | A stranger wrote for the first time. Fetch the requests. |
| `read` | `conversation, account, seq` | A member read up to `seq`. |
| `conversation` | `id` | A conversation is new or its members changed. Fetch it. |
| `conversation_gone` | `id` | This account is no longer in the conversation. |

- After `welcome`, and after every reconnect, a client reads what it may have missed: `GET /v1/party` and `GET /v1/party/invitations`, and `GET /v1/conversations` with the messages after the numbers it holds. News is a pointer or a numbered state and may be lost; reading the state is always right.
- The cloud pings every 20 seconds and closes a connection that does not answer.
- One device holds one event connection; a new one replaces the old.
- Close codes: 4001 the access token expired (refresh, then reconnect), 4003 banned, 4004 signed out. 1001 and 1012 mean the cloud's node is going away or restarting: reconnect at a random moment within the next 10 seconds, so that the clients of one node do not all come back at once. After any other break, a client waits half its pause plus a random part of up to the whole pause, starting at 1 second and doubling to at most 30.
- **Virtual addresses** come from `198.18.0.0/15` and belong to the members of a session, not to accounts ([ADR 0020](../docs/adr/0020-virtual-addresses-belong-to-sessions.md)). The cloud gives a joining account an address no other member of that session holds: the account's preferred address when it is free, else the next free one, never one ending in .0 or .255. The address stays with the member until it leaves, so every member of a session sees the same address for the same player for as long as the player is in it. A session never runs out of addresses (at most 64 members); a cloud's players online are not limited by addresses.
  - The **preferred address** of an account is the same every time, from any session, and is not reserved: another member of a session may hold it, and another session may give it to another player. `welcome.address` is the preferred address. A client takes its address in a session from the session's `members`, and uses `welcome.address` only for the time before it is in a session.
  - One address can name two players in two different sessions. A client in several sessions at once keeps a single address per peer across them (the net service does).
  - Clients from before session addresses keep working: they read `welcome.address` and `members[].address` and nothing else.

## Information

`GET /v1/info` needs no account and answers `{name, version, signup, privacy, terms, web}`. `signup` is `open` or `closed`; `privacy` and `terms` are URLs or null; `web` is the origin of the account web, and absent when the cloud has none.

## Limits

| What | Limit |
|---|---|
| `auth/email` | 3 per address and 10 per IP address per 15 minutes |
| `auth/code`, `auth/signup`, `auth/browser/token`, and the pages `web/authorize`, `web/code` and the passkey sign-in | 20 per IP address per 15 minutes |
| `auth/refresh` | 30 per device per hour |
| `relays/tickets` | 20 per account per hour |
| `PUT /v1/me/avatar` | 5 per account per hour, besides the limit below |
| messages | 30 per account per minute |
| conversations with strangers | 10 per account per hour |
| friend requests of a new account | 10 in its first day (`new_account_friend_requests_per_day`) |
| `PUT /v1/me/run-reports/{title_id}` | 30 per account per hour |
| `reports` | 20 per account per day and 5 per 10 minutes (settings of the cloud) |
| appeals | 10 per account per hour |
| everything else | 120 per account per minute |
