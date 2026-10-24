# Cloud API

The API between the net service and a cloud. Version 1 of the paths, still a draft: it changes without notice until the first public cloud runs.

The Rust reference is [`crates/api`](../crates/api). The cloud in [anyplay-gg/cloud](https://github.com/anyplay-gg/cloud) implements it. Nothing here knows the platform's formats; the net service translates those ([ADR 0001](../docs/adr/0001-bridge-forwards-net-service-translates.md)).

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
| `banned` | 403 | The account is banned. The body adds `until` (Unix seconds, absent for good). |
| `forbidden` | 403 | The account may not do this, for example because of a block. |
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
Device    { name, platform }                       platform: "windows" | "linux" | "macos" | "other"
Presence  { online, activity }                     activity: Activity or null
Activity  { title_id, title, version, session }    session: SessionRef or null
SessionRef{ id, joinable, members, max_members }
Friend    { account, since, presence }
Request   { account, since }
Session   { id, title_id, title, version, build, leader, max_members, visibility, properties, members, created }
Member    { account, endpoint, relay, address, joined }
Invitation{ id, from, session, title_id, title, version, expires }
Relay     { url, token, expires }
```

- A **handle** has 3 to 16 characters from `A-Z a-z 0-9 _ -` and starts with a letter. Handles are unique without regard to case; the spelling of the owner is kept. Some handles are reserved (names of the network, of staff roles, and of platforms).
- `visibility` is `friends` (friends of a member may join), `invite` (only invited accounts) or `closed`.
- `properties` is opaque data for the game, as standard base64, at most 8 KiB once decoded.
- `endpoint` is the net service's tunnel key (an iroh endpoint ID) as lowercase hex, and `relay` the URL of the relay it is reachable through, or null. Other members dial it with both; the cloud publishes neither anywhere else.
- `address` is the member's virtual address (see Events).
- `build` names the AnyPS5 build. Two players may only play together when `title_id`, `version` and `build` all match.

## Signing in

Accounts have no password. A device signs in with a code sent to the account's e-mail address and then keeps a refresh token ([ADR 0005](../docs/adr/0005-sign-in-by-e-mail-code.md)).

| Request | Body | Answer |
|---|---|---|
| `POST /v1/auth/email` | `{email}` | `{challenge, expires, code_length}` |
| `POST /v1/auth/code` | `{challenge, code, device}` | `{result: "signed_in", tokens, me}` or `{result: "new_account", signup, expires}` |
| `POST /v1/auth/signup` | `{signup, handle, age_confirmed, device}` | `{tokens, me}` |
| `POST /v1/auth/refresh` | `{refresh}` | `Tokens` |
| `POST /v1/auth/signout` | none (bearer) | 204 |

- `auth/email` answers the same whether or not an account exists, so it cannot be used to test addresses. A code is 6 digits, lives 10 minutes and allows 5 tries.
- `age_confirmed` must be `true`: the player confirms being old enough to agree alone (16 in the EU unless a country sets a lower age). Younger players need a parent's consent, which this version does not collect.
- An access token lives 15 minutes. A refresh token is replaced on every use. Using a replaced refresh token again signs the device out, because only a thief would do that.

## Accounts

| Request | Answer |
|---|---|
| `GET /v1/me` | `Me` |
| `DELETE /v1/me` | 204. Deletes the account with its devices, friends, requests, blocks and reports. |
| `GET /v1/me/export` | Everything the cloud stores about the account, as JSON. |
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

## Reports

`POST /v1/reports` with `{account, reason, details}` answers 204. `reason` is `cheating`, `harassment`, `spam`, `name` or `other`; `details` has at most 1,000 characters.

## Events

`GET /v1/events` upgrades to a WebSocket. The upgrade carries the bearer token. The cloud sends one JSON object per text message, each with a `type`:

| Type | Fields | Meaning |
|---|---|---|
| `welcome` | `address, server_time` | Sent first, once the cloud pushes to this connection. A client counts as online from here and fetches its lists after it, so no change falls between the two. `address` is this account's virtual address while it is online. |
| `friends` | | The friend lists changed; fetch `GET /v1/friends`. |
| `presence` | `account, presence` | A friend's presence changed. |
| `invitation` | `invitation` | A friend invited this account. |
| `invitation_gone` | `id` | An invitation expired, was declined or its session closed. |
| `session` | `session` | A session this account is in changed (members, leader, properties). |
| `session_gone` | `id` | This account is no longer in the session: it closed, or the leader removed it. |
| `signed_out` | | This device was signed out or the account deleted. Forget the tokens. |
| `banned` | `until` | The account was banned. |

- The cloud pings every 20 seconds and closes a connection that does not answer.
- One device holds one event connection; a new one replaces the old.
- Close codes: 4001 the access token expired (refresh, then reconnect), 4003 banned, 4004 signed out.
- **Virtual addresses** come from `198.18.0.0/15`. The cloud leases one to every online account, the same for the account each time when it is free, so every member of a session sees the same address for the same player.

## Information

`GET /v1/info` needs no account and answers `{name, version, signup, privacy, terms}`. `signup` is `open` or `closed`; `privacy` and `terms` are URLs or null.

## Limits

| What | Limit |
|---|---|
| `auth/email` | 3 per address and 10 per IP address per 15 minutes |
| `auth/code`, `auth/signup` | 20 per IP address per 15 minutes |
| `auth/refresh` | 30 per device per hour |
| everything else | 120 per account per minute |
