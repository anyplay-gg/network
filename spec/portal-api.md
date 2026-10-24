# Portal API

The JSON API that the web portal talks to: a single-page app that the cloud serves from its own origin, where players sign in, manage their account, and see friends, trophies and profiles. Version 1, still a draft: it changes without notice until the first public cloud runs.

The cloud in [anyplay-gg/cloud](https://github.com/anyplay-gg/cloud) implements it (`crates/cloud/src/web/api`). It is not the API of the net service ([Cloud API](cloud-api.md), `/v1`, bearer tokens) and not the platform web API that games call ([`web-api/`](web-api/README.md)). The decision is in [ADR 0018](../docs/adr/0018-the-portal-is-a-single-page-app-on-the-clouds-origin.md); the pages it replaces are the account web of [ADR 0016](../docs/adr/0016-accounts-are-managed-on-the-web.md).

## Conventions

- Every path starts with `/web/api/v1`. Bodies are JSON in UTF-8, at most 64 KiB. The one exception is the avatar upload, whose body is the picture.
- Times are Unix seconds. Account and device IDs are integers between 1 and 2^53 - 1.
- The portal and the API share one origin (the cloud's `public_url`). The portal calls the API with `fetch`, on the same origin. The cloud sends no CORS headers: no other site can read an answer.
- A request that needs a signed-in player carries the browser's session cookie. There are no bearer tokens here; the cloud answers `unauthorized` to a request without a live session.
- Objects that also exist in `/v1` keep their shape and are named in [Cloud API](cloud-api.md): `Me`, `Account`, `Profile`, `ProfileUpdate`, `Privacy`, `Count`, `FriendLists`, `FriendRequest`.

## Browser session and security

A browser has one session on the cloud, with its own cookie, apart from the tokens of devices. The cookie is `__Host-anyplay` on https (`anyplay_web` on a development cloud over http), `HttpOnly`, `SameSite=Strict`, `Secure` on https, `Path=/`. Only the SHA-256 of its value is stored. A session ends after 30 minutes without use, and after 4 hours at the latest. `GET /session` starts one when the browser has none.

Every answer carries the security headers of the account web: a content security policy that allows only the cloud's own scripts and styles (`script-src 'self'; style-src 'self'; frame-ancestors 'none'`), `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, `X-Frame-Options: DENY`, and `Cache-Control: no-store` (the cloud sets it unless the route says otherwise). The portal sets styles through the CSSOM, which the policy allows.

Four checks keep another site from using a signed-in browser. A write is a `POST`, `PUT`, `PATCH` or `DELETE`.

1. **Origin and fetch metadata.** A write whose `Origin` is not the cloud's origin, or whose `Sec-Fetch-Site` is not `same-origin` or `none`, is refused with 403 before anything else. (The same guard covers the HTML pages.)
2. **CSRF token.** Every write carries the session's token in `X-CSRF-Token`. The token comes from `GET /session` (`csrf`) and changes when the session does (signing in makes a new session; every answer that does so carries the new token). A missing or wrong token is `unauthorized`.
3. **JSON.** Every write has `Content-Type: application/json` (parameters such as `charset` are fine), even when it has no body: send `{}`. Any other content type, including the types a form can send, is `unsupported_media_type` (415). The picture of an avatar says `image/png` or `image/jpeg` instead.
4. **A live session.** The cookie must name a session that has not ended.

Reads need only the cookie. They change nothing.

### Step-up

Some actions give an attacker who holds a stolen session a lasting hold. They need a **fresh** session: one that signed in, or passed a step-up, in the last 15 minutes (`fresh_until` in the session view and in `GET /account` says when it ends). Without it the answer is

```text
403 {"error": "step_up_required", "message": "Confirm that it is you to do this."}
```

The actions that need it: starting or finishing a passkey registration, removing a passkey, the export of the data, signing out a device or another browser (a browser may always sign itself out), signing out everywhere, asking for or confirming a new e-mail address, and starting the deletion of the account. A thief with an old copy of a session can then neither read everything the cloud stores nor end the owner's sessions and keep the only one. Everything else needs only a session.

The page proves again who the player is in one of two ways, then repeats the action:

| Way | Requests | Answer |
|---|---|---|
| E-mail code | `POST /step-up/email`, then `POST /step-up/code` | `{fresh_until}` |
| Passkey | `POST /passkey/sign-in/start`, then `POST /step-up/passkey/finish` | `{fresh_until}` |

The session does not change: its cookie, its token and everything the page holds stay as they are. Only `fresh_until` moves.

## Errors

An error is an HTTP status with a body `{"error": "<code>", "message": "<English text>"}`, the shape of [Cloud API](cloud-api.md), with its codes. A problem with one input adds `field`. `Retry-After` comes with `rate_limited`. The codes that come up here:

| Code | Status | Meaning here |
|---|---|---|
| `bad_request` | 400 | The body is not valid, or a value is not allowed. With `field`: the input to mark. |
| `code_wrong` | 400 | The e-mail code is wrong. The step stays; five wrong tries end the challenge. |
| `code_expired` | 400 | The code or the sign-up is too old, or had too many tries. The flow starts over (or, for step-up and the e-mail change, the code is dropped). |
| `handle_invalid` | 400 | The handle breaks the rules. `field` is `handle`. |
| `unauthorized` | 401 | No live session, no or a wrong CSRF token, or the player is not signed in. The page reads `GET /session` and shows the sign-in. |
| `banned` | 403 | The account is banned. A signed-in session that meets this one leads to the statement (`flow.step` is `banned` from then on). When an address is banned and its account is gone, the body adds `ban` with the `statement`; it never holds an appeal token. |
| `forbidden` | 403 | The player may not do this (a closed cloud makes no new accounts; the passkey is registered already). |
| `step_up_required` | 403 | The action needs a fresh session. See Step-up. |
| `not_found` | 404 | There is nothing like that, or nothing the player may see. |
| `unsupported_media_type` | 415 | A write without `Content-Type: application/json`. |
| `handle_taken` | 409 | Another account has the handle. `field` is `handle`. |
| `limit` | 409 | A limit is reached (passkeys, friends, requests). |
| `too_large` | 413 | The body is too large. |
| `rate_limited` | 429 | Too many requests. Wait `Retry-After` seconds. |
| `internal` | 500 | The cloud failed. |

A request that the origin guard refuses (403 before the API sees it) has an HTML body, not JSON. A page that behaves does not meet it.

## Rate limits

Counted by the cloud's settings (`docs/configuration.md` of the cloud), per window:

- Asking for a code (`sign-in/email`, `sign-in/code/resend`, `step-up/email`, `account/email`): per address and per client address, as for `/v1/auth/email`. Typing a code and `passkey/*`: per client address, as for `/v1/auth/code`.
- A new browser session (`GET /session` without a live cookie): 300 per 15 minutes per client address, a bucket of its own, so many page loads behind one address (a shared network) cannot lock out signing in. A request that has its session is not counted. Every new session is a row in the store, because the CSRF token lives in it; the row ends with the session's idle time.
- Every other request of a signed-in session: per account, shared with `/v1` (120 per minute).
- Public profiles (`users/{name}...`): 120 per minute per client address.
- Avatar uploads: 5 per hour per account.

## The session and signing in

`GET /session` and every step of the flow answer with the **session view**:

```text
SessionView {
  csrf,              the token for X-CSRF-Token; null once the session is over
  signed_in,         true when the browser has an account
  account,           Me, or null
  fresh_until,       Unix seconds, or null when signed out
  flow,              FlowView
  authorize,         {device, create} while the app on the player's PC waits for this sign-in, or null
  notice,            a line to show once, or null
  cloud              {name, signup, privacy_url, terms_url}
}
FlowView { step, email?, create?, redirect?, terms? }
```

`flow.step` is what the page shows next:

| `step` | Meaning | Also in `flow` |
|---|---|---|
| `email` | Ask for an e-mail address (or a passkey). | `create`: the player wants a new account |
| `code` | A code is out; ask for it. | `email`, `create` |
| `details` | The address is verified and new; ask for a handle, age and terms. | `email`, `terms` |
| `terms` | The account is known; the terms changed; ask to accept. | `terms` |
| `banned` | The account is banned; show the statement (`GET /ban`). | |
| `handoff` | The sign-in is done for the app on the player's PC. | `redirect` |
| `done` | The browser is signed in. | |

`terms` is `{version, url}` and is present only when the cloud has terms.

```text
email ──► code ──┬──► done
                 ├──► handoff
                 ├──► terms ──► done | handoff
                 ├──► details ──► done | handoff | terms
                 └──► banned
passkey ─────────────► done | handoff | terms | banned
```

The steps are the state machine of the HTML pages ([`flow.rs`](https://github.com/anyplay-gg/cloud/blob/main/crates/cloud/src/web/flow.rs)): the same functions answer both.

### Hand-off to the app

When the launcher starts a sign-in it opens `/web/authorize` (checked as before: `client=net`, a loopback `redirect_uri`, an S256 challenge, a `state`) in the player's browser. The cloud keeps the request in a new browser session and redirects to the portal route `/sign-in` (`/create` for `intent=create`), or to the HTML page when the cloud serves no portal. `/web/authorize` and `/web/link` stay server routes.

When the player has signed in, the step is `handoff` and `flow.redirect` is the loopback address of the app with its one-time code and the `state`: `http://127.0.0.1:<port>/callback?code=...&state=...`. The page goes there with `location.assign(redirect)`; a form post would be held back by the policy's `form-action`. The app trades the code at `POST /v1/auth/browser/token`. The answer that carries `redirect` ends the browser session and clears the cookie (`csrf` is `null`): nothing of the sign-in stays in the browser. `GET /session` never ends anything. If a hand-off is waiting there (the player finished on the HTML pages), it shows `step: "handoff"` with the same `redirect`.

### Requests

| Request | Body | Answer |
|---|---|---|
| `GET /session` | | `SessionView`. Starts a session (and sets the cookie) when the browser has none. |
| `POST /sign-in/email` | `{email, intent}` | `SessionView`, step `code`. `intent` is `sign_in` (default) or `create`. The cloud sends the same mail whether or not the address has an account. |
| `POST /sign-in/code` | `{code}` | `SessionView`: `done`, `handoff`, `terms`, `details` or `banned`. A new address leads to `details`; a cloud with `signup = "closed"` answers `forbidden`. |
| `POST /sign-in/code/resend` | `{}` | `SessionView`, step `code`, `notice` "We sent a new code." The earlier code no longer counts. |
| `POST /sign-up/details` | `{handle, age_confirmed, terms_accepted}` | `SessionView`: `done`, `handoff` or `terms`. `age_confirmed` must be `true`; `terms_accepted` must be `true` when the cloud has terms. A problem names its input in `field`: `handle`, `age_confirmed` or `terms_accepted`. The cloud records the age and the terms version with the time. |
| `POST /terms` | `{accept}` | `SessionView`: `done` or `handoff`. `accept: false` is `bad_request`. |
| `POST /sign-out` | `{}` | `{signed_out: true}`. Ends the session and clears the cookie. |
| `POST /passkey/sign-in/start` | `{}` | `{id, options}`: the ceremony and the options for `navigator.credentials.get`, as webauthn-rs makes them (discoverable credentials, user verification required). |
| `POST /passkey/sign-in/finish` | `{id, credential}` | `SessionView`, like `sign-in/code`. `credential` is the browser's answer, as webauthn-rs reads it. A ceremony is used once. A passkey that does not work is `unauthorized`. |
| `GET /ban` | | The statement of the ban the session proved to be its own: `{until, rule, reason, decided, appeal}`. `appeal` is `{state: "none"}`, `{state: "open", deadline}`, `{state: "filed", status, answer}` (`status`: `open`, `upheld`, `reversed`) or `{state: "over"}`. `not_found` when the session has no ban to show. |
| `POST /ban/appeal` | `{text}` | `{filed}`. One appeal per action, 1 to 1,000 characters. |

## Account

All of these need a signed-in session that is not banned.

```text
Passkey  { id, name, created, last_used }          id: the credential ID, base64url; last_used: Unix seconds or null
Device   { id, name, platform, created, last_used }
Browser  { id, current, created, last_used }       id: names the session to its owner, not the cookie
```

| Request | Body | Answer |
|---|---|---|
| `GET /account` | | `{account, passkeys, max_passkeys, devices, sessions, deletion, pending_email, fresh_until}`. `account` is `Me`; `sessions` lists the browsers (`Browser`) and marks the one that asks; `deletion` is `{at}` or null; `pending_email` is an address waiting for its code, or null. |
| `POST /account/passkeys/start` | `{}` | `{id, options}` for `navigator.credentials.create`. Step-up. At most 10 passkeys (`limit`). |
| `POST /account/passkeys/finish` | `{id, name, credential}` | 201 `{passkey}`. Step-up. `name` is up to 40 characters; without one the cloud names it "Passkey n". |
| `PATCH /account/passkeys/{id}` | `{name}` | `{ok: true}`. |
| `DELETE /account/passkeys/{id}` | `{}` | `{ok: true}`. Step-up. Removing the last passkey is allowed: the e-mail code stays as the way in. |
| `DELETE /account/devices/{id}` | `{}` | `{ok: true}`. Step-up. Signs the device out at once and closes its connection. |
| `DELETE /account/sessions/{id}` | `{}` | `{ok: true, signed_out}`. Step-up, unless it is the browser's own session (then the cookie is cleared and `signed_out` is true). |
| `POST /account/sign-out-everywhere` | `{}` | `{signed_out: true}`. Step-up. Ends every device and every browser, this one too, and refuses every access token issued before. |
| `GET /account/export` | | Step-up. A JSON file (`Content-Disposition: attachment`) with everything the cloud stores about the account; the same document as `GET /v1/me/export`. |
| `POST /account/email` | `{email}` | `{step: "code", email}`. Step-up. Sends a code to the new address. An address that is invalid, the account's own or banned is `bad_request`. |
| `POST /account/email/confirm` | `{code}` | `{email}`. Step-up. The code makes the address the account's. If another account has it, `bad_request`, with the words of any other address that cannot be used: the answer does not say who has an address. Every device and every other browser is signed out (the browser that confirmed stays), and the old address gets a notice (see Mail). |
| `POST /account/deletion` | `{handle}` | `{deletion: {at}}`. Step-up. `handle` must be the account's own handle, any case (else `bad_request`, `field: "handle"`). |
| `DELETE /account/deletion` | `{}` | `{deletion: null}`. Cancels a scheduled deletion; harmless when there is none. From `at` on it is `forbidden` ("The grace period has ended"): the sweep is certain. |
| `POST /step-up/email` | `{}` | `{step: "code", email_hint}`. Sends a code to the account's own address. `email_hint` is `a***@example.com`. |
| `POST /step-up/code` | `{code}` | `{fresh_until}`. Without a code asked for, `bad_request`. |
| `POST /step-up/passkey/finish` | `{id, credential}` | `{fresh_until}`. The answer to the ceremony of `POST /passkey/sign-in/start`. A passkey of another account is `unauthorized`. |

### Deleting the account

The deletion waits for a **grace period** of 30 days, so a hijacked session, a stolen access token or a slip does not lose the account. **Every way to delete goes through it**: `POST /account/deletion`, `DELETE /v1/me` of the net service, and the HTML page `/web/account/delete`. Scheduling sets `at`, signs out every device, refuses the access tokens issued so far (the "not before" time moves) and ends every browser session but the one that asked; `DELETE /v1/me` is called by a device, so it ends them all. The account is not hidden meanwhile: friends still see it. The player cancels with `DELETE /account/deletion` until `at`. When `at` has passed, the sweep of every node deletes the account, whatever else happens; an account the sweep cannot delete is tried again ten minutes later, so a few failing rows do not hold up the others. Asking again keeps the first time. A player who signs in during the grace period (in the portal or for the launcher) sees `deletion` in `GET /account` and can cancel. The HTML pages cannot cancel; the portal can.

`GET /v1/me` does not show a scheduled deletion yet: `Me` has no field for it (see Open questions).

### Mail

Besides codes the cloud sends one kind of notice: when the account's address changes, the **old** address gets a plain-text mail (same sender as the codes) that names the new address and says what to do if it was not the player. It does not yet send a notice when a passkey is added or removed, or when a deletion is scheduled or cancelled (see Open questions).

## Friends

All of these need a signed-in session. They are the rules of `/v1`: the same code answers, so limits and effects are the same, including events to the devices of both players.

| Request | Body | Answer |
|---|---|---|
| `GET /friends` | | `FriendLists`: `friends` (with their presence), `incoming`, `outgoing` and `blocked`. |
| `POST /friends/requests` | `{handle}` | `{state}`: `requested`, or `friends` when they had asked first. A block stays private: the answer is the same. |
| `POST /friends/requests/{id}/accept` | `{}` | `{ok: true}`. `{id}` is the account that asked. |
| `DELETE /friends/requests/{id}` | `{}` | `{ok: true}`. Declines an incoming request or withdraws an outgoing one. |
| `DELETE /friends/{id}` | `{}` | `{ok: true}`. Ends the friendship. |
| `PUT /blocks/{id}` | `{}` | `{ok: true}`. |
| `DELETE /blocks/{id}` | `{}` | `{ok: true}`. |

Parties and messages are not in the portal yet.

## Profile, privacy and trophies

The player's own parts. Only fields the cloud has exist: the text, the favourite titles, the avatar, playtime (read-only, counted by the launcher), and the privacy of each part.

| Request | Body | Answer |
|---|---|---|
| `GET /me/profile` | | `Profile` of the player, all parts they have set. |
| `PATCH /me/profile` | `ProfileUpdate` | `Profile`. A part left out stays; an empty text or list clears it. The text has at most 160 characters without links; at most 3 favourites (title IDs). |
| `GET /me/privacy` | | `Privacy`: for each of `trophies`, `about`, `favourites`, `avatar`, `playtime`, who sees it: `friends` (the default), `everyone` or `nobody`. |
| `PUT /me/privacy` | `Privacy` | `Privacy`. |
| `PUT /me/avatar` | the picture | `{avatar}`: the hash. The body is a PNG or JPEG of at most 1 MB with `Content-Type: image/png` or `image/jpeg`; the cloud makes a square of it. The picture is at `GET /v1/avatars/{hash}` (no token; the hash is the right to see it). |
| `DELETE /me/avatar` | `{}` | `{ok: true}`. |
| `GET /me/trophies` | | `TrophySummary`. |

```text
TrophySummary { counts, titles }                    counts: Count (bronze, silver, gold, platinum)
TitleSummary  { title_id, counts, earned, last_earned }   titles: [TitleSummary], the title with the latest trophy first
```

The cloud keeps trophy records only: a title ID, the trophy's number, its grade and the time ([ADR 0009](../docs/adr/0009-trophy-records-not-trophy-texts.md)). Names, texts and art never reach it, so the portal shows counts and cannot show how many trophies a game has. The web shows no game art.

## Other players

These work without a session; with one, they know the viewer. **A part the viewer may not see is `null`**, as in `Profile`. The rule is one for the whole network ([ADR 0012](../docs/adr/0012-private-by-default.md)): the player always sees everything of themselves; friends see a part unless the player shows it to nobody; anyone else sees it only if the player shows it to everyone; A visitor who is not signed in is a stranger. **A block does not show on a public page**: a viewer whom the player blocked, and a viewer who blocked the player, see exactly what a stranger sees, with `relation: "none"`. Hiding more from them than from a visitor who is not signed in would tell them about the block. A block still keeps them from friend-only parts, from friend requests and from messages.

| Request | Answer |
|---|---|
| `GET /users/{name}` | `PublicUser`. `{name}` is a handle, in any case. |
| `GET /users/{name}/trophies` | `TrophySummary`; `not_found` when the viewer may not see the player's trophies, so it does not say whether there are any. |

```text
PublicUser { account, about, favourites, avatar, playtime, trophies, viewer }
  the Profile parts, each null when hidden or unset
  trophies   Count, or null when hidden
  viewer     null for a visitor who is not signed in, else {relation, requested, blocked}
    relation   "self", "friend" or "none"; a player who blocked the viewer shows as "none"
    requested  "incoming" (they asked the viewer), "outgoing" (the viewer asked them) or null
    blocked    the viewer blocked them
```

A handle that does not exist, and a name that cannot be a handle, are both `not_found`.

## Serving the portal

The cloud setting `portal_dir` (environment variable `ANYPLAY_CLOUD_PORTAL_DIR`) names the folder of the built app. It needs `public_url`, and the folder must hold `index.html`. Without it the cloud serves no portal, and `/web/authorize` sends the app's browser to the HTML pages.

With it:

- **The shell.** `index.html` answers `/`, `/sign-in`, `/create`, `/account` and everything below it, `/friends` and below, `/trophies` and below, and `/u/{name}`. It is sent with `Cache-Control: no-cache`.
- **Files.** `/assets/*` is served from the folder's `assets/`, with `Cache-Control: public, max-age=31536000, immutable`: the build names these files by the hash of their content. Other files at the top of the folder (the icon, a manifest) are served for an hour. A path that leaves the folder is `not_found`.
- **Nothing else changes.** `/v1`, `/web`, `/web/api/v1`, `/healthz`, `/readyz` and every other route of the cloud keep answering; a path that is neither a route of the app nor a file is the cloud's JSON `not_found`.
- **Headers.** The same security headers as the API, with the same policy. Pages load no script, style, font or picture from elsewhere.
- **Link previews.** The shell for `/u/{name}` carries `<title>`, `description` and Open Graph tags (`og:title`, `og:description`, `og:type`, `og:url`, `og:site_name`) made from the profile, HTML-escaped. They hold only what a visitor who is not signed in may see: the text appears only when the player shows it to everyone, else the description is "<handle> plays on <cloud name>." There is no image yet. A player who does not exist gets the plain shell with status 404.

## Open questions

- Mail notices for the other security changes (passkey added or removed, deletion scheduled or cancelled).
- `deletion_at` in `Me` (`GET /v1/me`), so the launcher can show a scheduled deletion. It needs a field in the API crate and a cloud that reads it.
- A stricter dormant state during the grace period: the account stays visible to friends now.
- Recovery codes, and a list of recent sign-ins with a rough place.
- Renaming a device, and the choice of a visibility preset for the whole profile.
- A feed and live updates for the portal. The events a launcher gets over its WebSocket have no portal counterpart yet.
- Compressed assets: the cloud sends files as they are; a proxy in front compresses.
