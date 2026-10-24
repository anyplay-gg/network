# Control protocol

The messages between the launcher and the net service it starts. Version 0, a draft: it changes without notice until version 1.

The Rust reference is [`crates/control`](../crates/control). The launcher shows; the net service acts ([ADR 0006](../docs/adr/0006-the-launcher-talks-to-the-net-service-over-its-standard-streams.md)).

## Connection

- The launcher starts the net service as a child process and talks to it over the child's standard input and output. Nobody else holds those streams, so the channel needs no secret.
- Each message is one JSON object on one line, in UTF-8, ended by `\n`. A line has at most 1 MiB.
- The net service writes its log to standard error. The launcher keeps it in its log folder.
- The net service ends when its standard input closes and no game is connected to it. While a game is still connected it waits for that game to end.

## Messages

The launcher sends **commands**; each has a number `id` and a name `do`:

```json
{"id": 7, "do": "add_friend", "handle": "Lumen"}
```

The net service answers every command once, with the same `id`, either `ok` or `error`:

```json
{"id": 7, "ok": {"result": "requested"}}
{"id": 7, "error": {"code": "not_found", "message": "No account has this handle."}}
```

It also sends **events** on its own, named by `event`:

```json
{"event": "state", "cloud": "online", "account": {"id": 41, "handle": "Alec", "email": "a@example.org"}, "pending": null}
```

Fields the receiver does not know are ignored, and so are events it does not know. A command it does not know is answered with the error `unknown_command`.

Error codes are those of the [cloud API](cloud-api.md), plus `unknown_command`, `offline` (the cloud cannot be reached), `not_signed_in`, `no_game` and `bad_state` (the command does not fit what is happening, such as a code without an e-mail before it).

## Commands

| `do` | Fields | `ok` |
|---|---|---|
| `hello` | `launcher` (its version), `cloud` (the cloud's URL) | `{service, protocol}` |
| `email` | `email` | `{code_length}` |
| `code` | `code` | `{result: "signed_in" \| "new_account"}` |
| `sign_up` | `handle`, `age_confirmed` | `{}` |
| `sign_out` | | `{}` |
| `delete_account` | | `{}` |
| `add_friend` | `handle` | `{result: "requested" \| "friends"}` |
| `accept` | `account` | `{}` |
| `decline` | `account` | `{}`: declines an incoming or withdraws an outgoing request |
| `remove_friend` | `account` | `{}` |
| `block` | `account` | `{}` |
| `unblock` | `account` | `{}` |
| `report` | `account`, `reason`, `details` | `{}` |
| `playing` | `game`: `{title_id, title, version}` or null | `{}` |
| `check_join` | `session`, `title_id`, `version`, `build` | `{}`, or the error `version_mismatch` |
| `game_start` | `title_id`, `title`, `version`, `build`, `join` (a session ID or null) | `{env}` |
| `game_end` | | `{}` |
| `invite` | `accounts` | `{sent}` |
| `answer_invitation` | `id`, `accept` | `{session, title_id}` when accepted, else `{}` |
| `dialog` | `id`, `accounts` (null when the player closed the picker) | `{}` |
| `relay_only` | `on` | `{}` |
| `upload_trophies` | `title_id`, `trophies` | `{added}` |
| `trophies` | `account` | `{titles}`, or the error `not_found` when they are hidden |
| `rarity` | `title_id` | `{title_id, players, trophies}` |
| `cards` | `accounts` | `{cards}` |
| `privacy` | | the player's `Privacy` |
| `set_privacy` | `privacy` | the new `Privacy` |
| `profile` | `account` | `Profile`: the parts the player may see of that account |
| `upload_playtime` | `entries` | `{changed}` |
| `party` | | `{party}`: the player's `Party`, or null |
| `party_invite` | `accounts` | `{sent}`: friends are invited; a party is made when the player is in none |
| `party_answer` | `id`, `accept` | `{party}` when accepted, else `{}` |
| `party_leave` | | `{}` |
| `party_kick` | `account` | `{}` (leader) |
| `party_leader` | `account` | `{}` (leader) |
| `party_move` | `session` | `{sent}`: invites the other members into the leader's session (leader) |
| `conversations` | | `{conversations, requests}` |
| `open_conversation` | `accounts`, `name` | the `Conversation`: with one account the direct one, with more a new group |
| `messages` | `conversation`, `after`, `before`, `limit` | `{messages, last}`; all but `conversation` may be left out |
| `send_message` | `conversation`, `text` | the `Message` |
| `mark_read` | `conversation`, `seq` | `{}` |
| `accept_request` | `conversation` | the `Conversation` |
| `leave_conversation` | `conversation` | `{}`: leaves a group or declines a request |
| `add_to_conversation` | `conversation`, `accounts` | `{added}` |

- `hello` comes first. Until it is answered every other command gets `bad_state`. The net service then resumes the device's saved sign-in, if there is one.
- `email` starts a sign-in; `code` finishes it or, for a new address, asks for `sign_up`.
- `game_start` answers the variables the launcher adds to the game's environment, at present `ANYPS5_NET_ENDPOINT` and `ANYPS5_NET_TOKEN` from the [bridge protocol](bridge-protocol.md). With `join`, the net service holds the join intent for that session. `game_end` follows when the game's process has ended.
- `invite` invites friends to the session the player is in.
- `profile` only reads. A player edits their profile, avatar and visibility on the cloud's web site, which uses the HTTP API directly ([cloud API](cloud-api.md#profiles-avatars-and-playtime)); the launcher shows what `profile` and `cards` answer. `upload_playtime` can be sent again with the same entries; the cloud keeps the highest minutes per title and day.
- The party and message commands are the [cloud API](cloud-api.md#parties) one to one, with the errors of its requests. `party_answer` and `party_leave` update the held party before the answer.
- `relay_only` keeps every tunnel on a relay, so other players never learn the PC's addresses, or lets them go direct again. Running tunnels start again and the peers reconnect, so the launcher sends it while no game runs. `anyplay-net --relay-only` starts with it on.

## Events

| `event` | Fields | When |
|---|---|---|
| `state` | `cloud`, `account`, `pending` | after `hello` and whenever one of them changes |
| `friends` | `friends`, `incoming`, `outgoing`, `blocked` | after sign-in and whenever a list or a friend's presence changes |
| `invitation` | `invitation` | a friend invited the player |
| `invitation_gone` | `id` | an invitation expired, was answered or its session closed |
| `session` | `session` or null | the session the player is in changed, began or ended |
| `invite_dialog` | `id`, `title`, `max` | the game opened its invitation dialog; answer with `dialog` |
| `notice` | `code`, `message` | something failed that no command waits for |
| `party` | `party` or null | the party the player is in changed, began or ended; a copy with an older revision is dropped before it gets here |
| `party_invitation` | `invitation` | the leader of a party invited the player |
| `party_invitation_gone` | `id` | an invitation expired, was answered or used up |
| `conversations` | `conversations`, `requests` | after sign-in and after every reconnect |
| `message` | `conversation`, `seq` | a new message; fetch it with `messages` |
| `message_request` | `conversation` | a stranger wrote for the first time |
| `read` | `conversation`, `account`, `seq` | a member read up to `seq` |
| `conversation` | `id` | a conversation is new or its members changed |
| `conversation_gone` | `id` | the player is no longer in the conversation |

- `cloud` is `offline`, `connecting` or `online`. `account` is `{id, handle, email}` or null. `pending` is `code` (an e-mail is out), `handle` (the account needs a handle) or null.
- `friends` carries the whole lists each time, with each friend's presence as in the cloud API. The net service sends at most four of them per second.
- `message` carries no text: the launcher keeps the highest number it has for each conversation and calls `messages` with `after`. The `conversations` event after a reconnect carries every conversation's newest number (`last`), so what arrived while the connection was down is fetched the same way.
- `session` is the cloud's `Session` without `build`, `properties` and the members' `endpoint`, and with one more field per member: `link`, which is `connecting`, `direct` or `relay` for the other players and null for the player.
