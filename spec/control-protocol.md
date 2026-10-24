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
| `browser_sign_in` | `create` (default false) | `{url}`: open it in the system browser |
| `cancel_sign_in` | | `{}` |
| `account_page` | | `{url}`: open it in the system browser |
| `email` (deprecated) | `email` | `{code_length}` |
| `code` (deprecated) | `code` | `{result: "signed_in" \| "new_account"}` |
| `sign_up` (deprecated) | `handle`, `age_confirmed` | `{}` |
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
| `voice` | `on` | `{}` |
| `push_to_talk` | `talking` | `{}` |
| `voice_mode` | `mode`: `push_to_talk` or `voice_activity` | `{}` |
| `peer_volume` | `account`, `percent` (0 to 200) | `{}`; `bad_request` above 200 |
| `peer_mute` | `account`, `muted` | `{}` |
| `voice_devices` | | `{input, output, selected_input, selected_output}`: names; a selected name is null for the system's default |
| `set_voice_device` | `kind`: `input` or `output`, `name` (null: the system's default) | `{}`; `not_found` when no device has the name |
| `conversations` | | `{conversations, requests}` |
| `open_conversation` | `accounts`, `name` | the `Conversation`: with one account the direct one, with more a new group |
| `messages` | `conversation`, `after`, `before`, `limit` | `{messages, last}`; all but `conversation` may be left out |
| `send_message` | `conversation`, `text` | the `Message` |
| `mark_read` | `conversation`, `seq` | `{}` |
| `accept_request` | `conversation` | the `Conversation` |
| `leave_conversation` | `conversation` | `{}`: leaves a group or declines a request |
| `add_to_conversation` | `conversation`, `accounts` | `{added}` |

- `hello` comes first. Until it is answered every other command gets `bad_state`. The net service then resumes the device's saved sign-in, if there is one.
- `browser_sign_in` is how the launcher signs a player in or creates an account ([ADR 0016](../docs/adr/0016-accounts-are-managed-on-the-web.md)). The net service opens a listener on `127.0.0.1` with a random port, makes a PKCE verifier and a `state`, and answers the address of the cloud's sign-in page; the launcher opens it in the system browser. The net service then sends a `state` event with `pending` `browser`, and when the player has finished in the browser, a `state` event with the account (and `pending` null). It fails with `not_found` when the cloud has no account web, and with `bad_state` when an account is signed in. A browser that is not finished in 10 minutes, or a sign-in the player stops, ends with a `notice` (`sign_in_timed_out`, `sign_in_stopped`) and `pending` null. `create` opens the page at "Create account". Nothing is typed in the launcher.
- `cancel_sign_in` ends a `browser_sign_in` that is waiting. It answers `{}` even when none is.
- `account_page` answers a link that opens the account web signed in as the player (60 seconds, one use): e-mail, passkeys, devices, a copy of the data, deleting the account. It needs a signed-in account whose device has a key (`unauthorized` otherwise: sign in again) and a cloud with an account web (`not_found`).
- `email`, `code` and `sign_up` still work and are deprecated: use `browser_sign_in`. `email` starts a sign-in; `code` finishes it or, for a new address, asks for `sign_up`.
- `game_start` answers the variables the launcher adds to the game's environment, at present `ANYPS5_NET_ENDPOINT` and `ANYPS5_NET_TOKEN` from the [bridge protocol](bridge-protocol.md). With `join`, the net service holds the join intent for that session. `game_end` follows when the game's process has ended.
- `invite` invites friends to the session the player is in.
- `profile` only reads. A player edits their profile, avatar and visibility on the cloud's web site, which uses the HTTP API directly ([cloud API](cloud-api.md#profiles-avatars-and-playtime)); the launcher shows what `profile` and `cards` answer. `upload_playtime` can be sent again with the same entries; the cloud keeps the highest minutes per title and day.
- The party and message commands are the [cloud API](cloud-api.md#parties) one to one, with the errors of its requests. `party_answer` and `party_leave` update the held party before the answer.
- The voice commands work on [party voice](voice.md). `voice` with `on` true switches it on: from then on, whenever the player is in a party and signed in with tunnels up, the net service opens the microphone and speakers, joins the party's voice and tells the party it takes part. Switched off, or without a party, the devices are closed: the microphone is never open otherwise. The setting lasts until `voice` is sent again or the service ends; a new service starts with voice off, so the launcher sends `voice` after `hello` if the player wants it. The answer does not wait for the devices: if they cannot be opened, the net service switches voice off again, sends a `notice` `voice_device` with the reason and a `voice` event with `enabled` false.
- `push_to_talk` is the key being down (`talking` true) or up. In `voice_activity` mode it has no effect. `voice_mode`, `peer_volume` and `peer_mute` are remembered by account for as long as the service runs, also while voice is off or the member is away, and apply when voice starts. Volume is in percent of what was sent, 100 normally.
- `voice_devices` lists the names the system has and which are chosen. `set_voice_device` chooses by one of those names (or null for the default) and, while voice runs, closes the devices and opens them again, so the connections to the members are made again too. A service built without sound devices lists none and fails to open any; release builds have them.
- Share play adds commands and events, drafted in the [share play spec](share-play.md#control-protocol-additions). They are not implemented.
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
| `voice` | `state`, `enabled`, `mode`, `me`, `peers` | the voice state changed, or while voice runs a level or a link changed (at most ten a second) |
| `party_invitation` | `invitation` | the leader of a party invited the player |
| `party_invitation_gone` | `id` | an invitation expired, was answered or used up |
| `conversations` | `conversations`, `requests` | after sign-in and after every reconnect |
| `message` | `conversation`, `seq` | a new message; fetch it with `messages` |
| `message_request` | `conversation` | a stranger wrote for the first time |
| `read` | `conversation`, `account`, `seq` | a member read up to `seq` |
| `conversation` | `id` | a conversation is new or its members changed |
| `conversation_gone` | `id` | the player is no longer in the conversation |

- `cloud` is `offline`, `connecting` or `online`. `account` is `{id, handle, email}` or null. `pending` is `code` (an e-mail is out), `handle` (the account needs a handle), `browser` (the system browser has the sign-in) or null.
- `friends` carries the whole lists each time, with each friend's presence as in the cloud API. The net service sends at most four of them per second.
- `message` carries no text: the launcher keeps the highest number it has for each conversation and calls `messages` with `after`. The `conversations` event after a reconnect carries every conversation's newest number (`last`), so what arrived while the connection was down is fetched the same way.
- `voice.state` is `off` (switched off, no party, or the devices could not be opened), `connecting` (switched on in a party: the devices are opening, or members who take part are not reached yet) or `on` (the devices run and at least one member who takes part is reached, or none takes part). `enabled` is whether the player switched voice on, which stays true across parties. `mode` is `push_to_talk` or `voice_activity`. `me` is `{level, speaking}` for the player's own microphone after the gate. `peers` has one entry for each other member of the party: `{account, level, speaking, link}`, with `link` `connecting`, `direct` or `relay` for the voice connection to that member and null when there is none (they have voice off, or have not been reached). `level` is the recent loudness in percent (0 to 100) before the player's volume setting; `speaking` is what a speaking indicator should show. While voice is off, `me` is silent and `peers` is empty. While voice runs, the net service reports levels and links only when they changed.
- The `party` event and the party answers leave out the members' tunnel keys and relays (`endpoint` is `""`, `relay` null), as `session` does; each member's `voice` says who takes part in voice.
- `session` is the cloud's `Session` without `build`, `properties` and the members' `endpoint`, and with one more field per member: `link`, which is `connecting`, `direct` or `relay` for the other players and null for the player.
