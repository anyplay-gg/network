# Web API group `communicationRestrictionStatus`

Status: **offered**, with a body shape that is a guess until a game we own has accepted it. The shape is established only by observing what a game we own does with an answer, never from anyone's documentation or code ([ADR 0021](../../docs/adr/0021-the-line-is-drawn-at-the-law.md)).

## Observed

| Game | Build | Run | Account |
|---|---|---|---|
| Overcooked! All You Can Eat | AnyPS5 with the bridge ([anyplaynet/AnyPS5#14](https://github.com/anyplaynet/AnyPS5/pull/14), [#15](https://github.com/anyplaynet/AnyPS5/pull/15)) | to the host screen | signed in to an AnyPlay cloud |

## Requests

### `GET /v3/users/{user}/communication/restriction/status`

| Part | Seen |
|---|---|
| `{user}` | the signed-in account's ID in decimal |
| Headers | `X-Psn-NpCppWebApi-Context: PS5/1.00` |
| Body | none |
| How often | about 10 times a second, after the player chooses to host and before any session or signaling call |

## What the game did

- **"Not offered"** (status 0): the game showed error E011-9 and asked again, about 10 times a second.

## What the net service answers

Only for `GET` on this path, only while a player is signed in, and only when `{user}` is the account ID the bridge reports as the signed-in user (or `me`). Every other request of the group (another ID, signed out, another path or method) stays "not offered".

The answer comes from the AnyPlay account's own restrictions, the same list the launcher shows (`GET /v1/me/actions` of the cloud, see its `docs/moderation.md`): a **mute** or a **ban** that has not been lifted and has not run out. A warning, and a new account's limit on writing to strangers, do not count: that account may still play and host.

| State | Status | Body |
|---|---|---|
| Not restricted | 200 | `{"restricted":false}` |
| Restricted | 200 | `{"restricted":true,"until":<unix seconds>}`; `until` is absent while a restriction is for good |

The restrictions are not read per request. The net service already holds them: it reads them on going online and when the cloud pushes an `action` event, and in addition once when a request finds the copy older than 60 seconds. The end time is checked on every answer, so a mute that runs out stops restricting without a new read.

Each answer is logged at debug level (`ANYPLAY_NET_LOG=anyplay_net::webapi=debug`) with a short hash of the account ID, the state and the status. Nothing else about the player.

## Trying other body shapes (testers)

The body shape is unknown. Point the net service at a JSON file with `ANYPLAY_NET_WEBAPI_OVERRIDES=<path>` (off when unset; for testing only). The file maps the group to the two states and gives the whole reply for each; a state without an entry keeps the default above:

```json
{
  "communicationRestrictionStatus": {
    "notRestricted": { "status": 200, "headers": { "Content-Type": "application/json" }, "body": { "restricted": false } },
    "restricted":    { "status": 200, "body_text": "{\"restricted\":true}" }
  }
}
```

- `status` defaults to 200, `headers` to `Content-Type: application/json`.
- `body` is any JSON value, written compactly; `body_text` is sent byte for byte (an empty string for an empty body) and wins over `body`.
- The file is read on every request: edit it while the game runs. A file that cannot be read or parsed is logged as a warning and the default answer is used.
- Write down in this file what the game did with each shape you tried.

## Unknown

- The body fields, status and headers the game accepts, and whether it reads the body at all.
- Whether the game sends the same request for other users (friends, session members) and what it wants then.
- What the game does when the answer is "restricted".
