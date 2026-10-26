# Control protocol

The messages between the launcher and the net service it starts. Version 0, a draft: it changes without notice until version 1.

The Rust reference is [`crates/control`](../crates/control). The launcher shows; the net service acts ([ADR 0006](../docs/adr/0006-the-launcher-talks-to-the-net-service-over-its-standard-streams.md)).

## Connection

- The launcher starts the net service as a child process and talks to it over the child's standard input and output. Nobody else holds those streams, so the channel needs no secret.
- Each message is one JSON object on one line, in UTF-8, ended by `\n`. A line has at most 1 MiB.
- The launcher sets `ANYPLAY_CLIENT` in the net service's environment to `launcher/<its version>` (such as `launcher/0.3.0`). The net service sends it to the cloud as `X-AnyPlay-Client` on every request and on the event connection ([cloud API](cloud-api.md#conventions)); without it, or when it is not a valid value, it sends `net/<its own version>`, which no cloud gates.
- The net service writes its log to standard error. The launcher keeps it in its log folder.
- The net service ends when its standard input closes and no game is connected to it. While a game is still connected it waits for that game to end.

## Messages

The launcher sends **commands**; each has a number `id` and a name `do`:

```json
{"id": 7, "do": "add_friend", "handle": "Lumen"}
```

No command has a field of its own named `id`, since it would take the place of the number. A command about an invitation or a dialog names it `invitation` or `dialog`.

The net service answers every command once, with the same `id`, either `ok` or `error`:

```json
{"id": 7, "ok": {"result": "requested"}}
{"id": 7, "error": {"code": "not_found", "message": "No account has this handle."}}
```

It also sends **events** on its own, named by `event`:

```json
{"event": "state", "cloud": "online", "account": {"id": 41, "handle": "Alec", "email": "a@example.org", "appear_offline": false}, "pending": null}
```

Fields the receiver does not know are ignored, and so are events it does not know. A command it does not know is answered with the error `unknown_command`.

Error codes are those of the [cloud API](cloud-api.md), plus `unknown_command`, `offline` (the cloud cannot be reached), `not_signed_in`, `no_game` and `bad_state` (the command does not fit what is happening, such as a code without an e-mail before it).

An error can carry more than a code and a message. `version_mismatch` adds `expected`. `restricted` and `banned` add `restriction`, so the launcher can say what is limited and until when instead of showing a generic failure:

```json
{"id": 9, "error": {"code": "restricted", "message": "A moderator muted this account.", "restriction": {"kind": "muted", "until": 1793000000, "rule": "spam", "reason": "Links in every message.", "action": 41, "appeal": {"status": "available", "deadline": 1808000000}}}}
```

A **restriction** has a `kind` and, when they apply, `until`, `rule`, `reason`, `action` and `appeal`. It is the cloud's statement of reasons ([cloud API](cloud-api.md#moderation)) reduced to what the player reads; it never names a reporter or a moderator, and the appeal token of a ban stays in the net service.

| `kind` | Meaning | Comes with |
|---|---|---|
| `muted` | A moderator muted the account: no friend requests and no messages. | `until`, `rule`, `reason`, `action`, `appeal` |
| `new_account` | The account is too new to write to people who are not friends, or to send more friend requests today. Nothing a moderator did. | `until` (when it may) |
| `banned` | The account is banned and signed out. | `until` (absent for good), and when the cloud has a statement `rule`, `reason`, `action`, `appeal` |

When the cloud retires this launcher's version ([cloud API](cloud-api.md#conventions)), a command that reaches the cloud fails with `client_too_old`, which adds `minimum` (the oldest version the cloud accepts, as a string):

```json
{"id": 9, "error": {"code": "client_too_old", "message": "This launcher is too old. Update to 0.3.0 or newer.", "minimum": "0.3.0"}}
```

`kind` is a plain string and `appeal.status` too (`available`, `open`, `upheld`, `reversed`, `expired`): a launcher treats a value it does not know as a restriction it cannot name, and still shows `until` and `reason`. A `restricted` failure from a net service that predates this field has none; the launcher then falls back to the `message`.

## Commands

| `do` | Fields | `ok` |
|---|---|---|
| `hello` | `launcher` (its version), `cloud` (the cloud's URL) | `{service, protocol}` |
| `browser_sign_in` | `create` (default false) | `{url}`: open it in the system browser |
| `cancel_sign_in` | | `{}` |
| `account_page` | | `{url}`: open it in the system browser |
| `appeal` | `action` (a restriction's `action`), `text` (1 to 1,000 characters) | `{}`; the `restrictions` event follows with the appeal's new status. `limit` when the action was appealed before, `forbidden` after its deadline, `not_found` for an action that is not the player's |
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
| `check_join` | `session`, `title_id`, `version`, `build`, `version_rule` (`exact`, `major` or `any`; absent is `exact`), `override_version` (true for "Try Anyway"; absent is false) | `{}`, or the error `version_mismatch` |
| `game_start` | `title_id`, `title`, `version`, `build`, `join` (a session ID or null), `host` (Play Together: `{max_members, visibility, party}`, or absent), `version_rule` and `override_version` (as in `check_join`, passed on to the cloud when `join` is set) | `{env, session}` |
| `game_end` | | `{}` |
| `invite` | `accounts` | `{sent}` |
| `answer_invitation` | `invitation` (its `id`), `accept` | `{session, title_id}` when accepted, else `{}` |
| `dialog` | `dialog` (the `id` of the `invite_dialog` event), `accounts` (null when the player closed the picker) | `{}` |
| `discord_status` | | `{state, user, friends}`: see the `discord` event |
| `discord_connect` | | `{}` at once; the `discord` event follows. `not_signed_in`, and `bad_state` when this build has no Discord SDK |
| `discord_disconnect` | | `{}`: ends the connection and forgets the kept refresh token |
| `relay_only` | `on` | `{}` |
| `upload_trophies` | `title_id`, `trophies` | `{added}` |
| `trophies` | `account` | `{titles}`, or the error `not_found` when they are hidden |
| `rarity` | `title_id` | `{title_id, players, trophies}` |
| `compare` | `account`, `title_id` | `TrophyCompare` of [Comparing](cloud-api.md#trophies): the player (`you`) next to that account (`them`) in one title, with points and level per side; ids only, the launcher adds names and art from the game's own trophy file. `not_found` when that account does not exist or its trophies are hidden; `bad_request` for the player's own account |
| `secret_trophies` | `title_id`, `numbers` | `{changed}`: marks the trophies as secret (a game hides them; the launcher knows which); idempotent |
| `reveal_trophies` | `title_id`, `numbers` | `{changed}`: shows secret trophies to everyone who may see the player's trophies; there is no way back |
| `run_report` | `title_id`, `version`, `toolkit`, `verdict` (`no_start`, `menus`, `in_game`, `playable`) | `{}`; the service adds the player's system |
| `run_reports` | `title_id`, `version` (or null) | `{title_id, version, reports, verdict, counts}` |
| `cards` | `accounts` | `{cards}` |
| `set_appear_offline` | `on` | `{appear_offline}`: the new setting, see [Appearing offline](cloud-api.md#appearing-offline); `offline` when the cloud cannot be reached (nothing is changed then) |
| `privacy` | | the player's `Privacy` |
| `set_privacy` | `privacy` | the new `Privacy` |
| `profile` | `account` | `ProfilePage`: the `Profile` with the parts the player may see of that account, their trophy counts (null when hidden) and the connections they show (`links`) |
| `upload_playtime` | `entries` | `{changed}` |
| `feed` | `before`, `limit` | `{items, next}`: a page of the [feed](cloud-api.md#feed), newest first; both fields may be left out |
| `react` | `item`, `reaction` | `{reactions, mine}`; `reaction` is `nice`, `gg`, `wow` or `same`, or null to take the reaction back. `not_found` when the player cannot see the item. (The item is named `item`: `id` is the number of the command.) |
| `party` | | `{party}`: the player's `Party`, or null |
| `party_invite` | `accounts`, `call` (default false) | `{sent}`: friends are invited; a party is made when the player is in none. With `call` they are rung: the invitation lives 30 seconds |
| `party_link` | `url` | `{}`: the leader sets the party's Discord link; the empty string removes it. `bad_request` for a link that is not a Discord one |
| `missed_calls` | | `{missed}` |
| `calls_seen` | `account` (null: all) | `{}` |
| `party_answer` | `invitation` (its `id`), `accept` | `{party}` when accepted, else `{}` |
| `party_leave` | | `{}` |
| `party_kick` | `account` | `{}` (leader) |
| `party_leader` | `account` | `{}` (leader) |
| `party_move` | `session` | `{sent}`: invites the other members into the leader's session (leader) |
| `join_code` | | `{code, url, expires}`: a new [join code](cloud-api.md#join-codes) for the player; `not_found` when there is nothing to join |
| `join_code_revoke` | | `{}` |
| `join_redeem` | `code` | `{kind, invitation}`: the invitation the code made, to be shown like any other (`kind` is `session` or `party`) |
| `voice` | `on` | `{}` |
| `mic_mute` | `muted` | `{}`: mutes the player's own microphone in party voice, whatever the mode |
| `deafen` | `deafened` | `{}`: nobody is heard, and the microphone is silenced too |
| `push_to_talk` | `talking` | `{}` |
| `voice_noise` | `enabled` | `{}`: noise suppression (RNNoise) on the microphone; on in a new service |
| `voice_mode` | `mode`: `push_to_talk` or `voice_activity` | `{}` |
| `peer_volume` | `account`, `percent` (0 to 200) | `{}`; `bad_request` above 200 |
| `peer_mute` | `account`, `muted` | `{}` |
| `voice_devices` | | `{input, output, selected_input, selected_output}`: names; a selected name is null for the system's default |
| `set_voice_device` | `kind`: `input` or `output`, `name` (null: the system's default) | `{}`; `not_found` when no device has the name |
| `conversations` | | `{conversations, requests}` |
| `open_conversation` | `accounts`, `name` | the `Conversation`: with one account the direct one, with more a new group |
| `messages` | `conversation`, `after`, `before`, `changed_after`, `limit` | `{messages, last, rev}`; all but `conversation` may be left out, and at most one of the first three given |
| `send_message` | `conversation`, `text`, `reply` | the `Message`; `reply` (optional) is the number of the message it answers |
| `edit_message` | `conversation`, `seq`, `text` | the `Message`, within 15 minutes of writing it |
| `unsend_message` | `conversation`, `seq` | the `Message` with `deleted` true |
| `react_message` | `conversation`, `seq`, `reaction` | the `Message`; `reaction` is `nice`, `love`, `laugh`, `gg`, `wow`, `sad`, or null to take it back |
| `typing` | `conversation`, `typing` | `{}`: the net service passes on at most one `true` every 3 seconds per conversation |
| `conversation_settings` | `conversation`, `muted`, `pinned` | the `Conversation`; a field left out stays |
| `mark_read` | `conversation`, `seq` | `{}` |
| `accept_request` | `conversation` | the `Conversation` |
| `leave_conversation` | `conversation` | `{}`: leaves a group or declines a request |
| `add_to_conversation` | `conversation`, `accounts` | `{added}` |
| `screen_start` | `source`: `game` (default) or `test_pattern`; `window`: the game's window as a number (a Windows `HWND`), `pids`: the game's process ids, both optional | `{}`: the player's game is streamed to the party |
| `screen_stop` | | `{}` |
| `screen_watch` | `account` | `{}`: watch the stream that member offers |
| `screen_leave` | | `{}` |
| `screen_worker` | `path` | `{encoders, decoders, capture, libraries}`: where the launcher put the verified share worker; `libraries` (absent when the worker loads none) is `[{name, state, detail}]`, see [Screen share](screen-share.md#control-protocol-additions) |

- `hello` comes first. Until it is answered every other command gets `bad_state`. The net service then resumes the device's saved sign-in, if there is one.
- `browser_sign_in` is how the launcher signs a player in or creates an account ([ADR 0016](../docs/adr/0016-accounts-are-managed-on-the-web.md)). The net service opens a listener on `127.0.0.1` with a random port, makes a PKCE verifier and a `state`, and answers the address of the cloud's sign-in page; the launcher opens it in the system browser. The net service then sends a `state` event with `pending` `browser`, and when the player has finished in the browser, a `state` event with the account (and `pending` null). It fails with `not_found` when the cloud has no account web, and with `bad_state` when an account is signed in. A browser that is not finished in 10 minutes, or a sign-in the player stops, ends with a `notice` (`sign_in_timed_out`, `sign_in_stopped`) and `pending` null. `create` opens the page at "Create account". Nothing is typed in the launcher.
- `cancel_sign_in` ends a `browser_sign_in` that is waiting. It answers `{}` even when none is.
- `account_page` answers a link that opens the account web signed in as the player (60 seconds, one use): e-mail, passkeys, devices, a copy of the data, deleting the account. It needs a signed-in account whose device has a key (`unauthorized` otherwise: sign in again) and a cloud with an account web (`not_found`).
- `email`, `code` and `sign_up` still work and are deprecated: use `browser_sign_in`. `email` starts a sign-in; `code` finishes it or, for a new address, asks for `sign_up`.
- `game_start` answers the variables the launcher adds to the game's environment, at present `ANYPS5_NET_ENDPOINT` and `ANYPS5_NET_TOKEN` from the [bridge protocol](bridge-protocol.md). With `join`, the net service holds the join intent for that session. `game_end` follows when the game's process has ended.
- `game_start` with `host` is **Play Together** ([spec](play-together.md)): before it answers, the net service opens a `lan` session for the game with `max_members` (2 to 64) and `visibility` (`friends` or `invite`), and answers its ID in `session`. With `party` true it also asks the player's party in: the party moves into the session when the player leads it, else each other member gets an invitation (the cloud delivers it to friends only). `join` wins when both are given. If the session cannot be opened (the network is not ready, the player is in the most sessions allowed, the cloud answered an error) the game starts all the same, `session` is absent, and a `notice` with the code `host_failed` and the reason follows at once. The game is then a game like any other, and the launcher says why nobody can join.
- `invite` invites friends to the session the player is in.
- `profile` only reads. A player edits their profile, avatar and visibility on the cloud's web site, which uses the HTTP API directly ([cloud API](cloud-api.md#profiles-avatars-and-playtime)); the launcher shows what `profile` and `cards` answer. `upload_playtime` can be sent again with the same entries; the cloud keeps the highest minutes per title and day.
- The party and message commands are the [cloud API](cloud-api.md#parties) one to one, with the errors of its requests. `party_answer` and `party_leave` update the held party before the answer.
- The join commands are the [cloud API](cloud-api.md#join-codes) one to one, with the errors of its requests. `join_redeem` takes the code alone or a whole link (`anyps5://v1/join/<code>`, `https://…/join/<code>`) and reads the code out of it; text that holds no code is `bad_request`. The invitation also arrives as the usual `invitation` or `party_invitation` event. Neither command joins anything.
- The voice commands work on [party voice](voice.md). `voice` with `on` true switches it on: from then on, whenever the player is in a party and signed in with tunnels up, the net service opens the microphone and speakers, joins the party's voice and tells the party it takes part. Switched off, or without a party, the devices are closed: the microphone is never open otherwise. The setting lasts until `voice` is sent again or the service ends; a new service starts with voice off, so the launcher sends `voice` after `hello` if the player wants it. The answer does not wait for the devices: if they cannot be opened, the net service switches voice off again, sends a `notice` `voice_device` with the reason and a `voice` event with `enabled` false.
- `push_to_talk` is the key being down (`talking` true) or up. In `voice_activity` mode it has no effect. `voice_mode`, `peer_volume` and `peer_mute` are remembered by account for as long as the service runs, also while voice is off or the member is away, and apply when voice starts. Volume is in percent of what was sent, 100 normally.
- `voice_devices` lists the names the system has and which are chosen. `set_voice_device` chooses by one of those names (or null for the default) and, while voice runs, closes the devices and opens them again, so the connections to the members are made again too. A service built without sound devices lists none and fails to open any; release builds have them.
- Share screen (watch only, to the party, no cloud involved) is specified in [its own file](screen-share.md#control-protocol-additions): the `screen_*` commands and the `screen` event. `screen_start` fails with `no_game`, `not_signed_in`, `not_in_party`, `no_encoder` or `busy`; `screen_watch` with `not_found`, `busy` or `no_decoder`. The event carries `hosting` (the player's own stream with one entry per member it reaches), `offers` (streams the player may watch) and `watching`, and no field of it is named `id`.
- The Discord commands and event are phase 0 of [ADR 0022](../docs/adr/0022-discord-social-sdk.md). `state` is `unavailable` (no SDK in this build, or the library is missing or refused to load: nothing else changes), `signed_out`, `connecting` (the SDK asks the player, the cloud trades the code, or the SDK connects) or `connected`. `user` is `{id, name}` while connected (`id` is a decimal string, `name` the display name) and `friends` the number of Discord friends; both are null otherwise, and the friends themselves never leave the net service. `discord_connect` uses the refresh token the service kept in the credential store for this account, and only asks the player (Discord's overlay, app or browser) when there is none or Discord refuses it; the cloud keeps nothing ([cloud API](cloud-api.md#discord-social-sdk)). A failure comes as a `notice` with the code `discord_failed`.
- Share play adds commands and events, drafted in the [share play spec](share-play.md#control-protocol-additions). They are not implemented.
- `appear_offline` is a setting of the account, so a launcher needs one command and one field: `set_appear_offline` with `on` writes it to the cloud, and `account.appear_offline` of the `state` event (sent after sign-in, after the command, and when another device or the portal changes it) says what it is. The player's own friends list and presence stay as they are; the launcher shows its "Appearing offline" hint from this field. The cloud stops friends from seeing the player as online; the net service does not hide anything itself, and keeps the event connection and every other command working.
- `relay_only` keeps every tunnel on a relay, so other players never learn the PC's addresses, or lets them go direct again. Running tunnels start again and the peers reconnect, so the launcher sends it while no game runs. `anyplay-net --relay-only` starts with it on.

## Events

| `event` | Fields | When |
|---|---|---|
| `state` | `cloud`, `account`, `pending` | after `hello` and whenever one of them changes |
| `discord` | `state`, `user`, `friends` | after `hello` and whenever the Discord state changes |
| `friends` | `friends`, `incoming`, `outgoing`, `blocked` | after sign-in and whenever a list or a friend's presence changes |
| `invitation` | `invitation` | a friend invited the player |
| `invitation_gone` | `id` | an invitation expired, was answered or its session closed |
| `session` | `session` or null | the session the player is in changed, began or ended |
| `invite_dialog` | `id`, `title`, `max` | the game opened its invitation dialog; answer with `dialog` |
| `session_ended` | `session`, `title_id`, `title`, `host` (an Account) | a Play Together session closed, or took the player out, while the player's game runs. Not sent when the player's own game ended, or for a session the player leads. The game is not touched |
| `notice` | `code`, `message` | something failed that no command waits for |
| `restrictions` | `restrictions` | what limits the signed-in account now: a list of restrictions, empty when nothing does. Sent after the account goes online, when a moderator mutes or warns it, when an appeal is filed or decided, and after a `restricted` failure the list did not contain |
| `banned` | `restriction` | the account was banned (the event connection saw it, or a request answered `banned`); the account is signed out. A `notice` with the code `banned` is sent too, for launchers that predate this event |
| `client_too_old` | `minimum` | the cloud refuses this launcher because its version is below `minimum`. Sent once per run of the net service, by a request or by the event connection, whichever finds out first |
| `party` | `party` or null | the party the player is in changed, began or ended; a copy with an older revision is dropped before it gets here |
| `missed_calls` | `missed` | after sign-in and after every reconnect: every call nobody answered |
| `missed_call` | `call` | a friend called and the player did not answer |
| `missed_call_gone` | `account` | the missed calls from that account were seen |
| `call_ended` | `account`, `reason` | a call the player placed ended unanswered (`declined`, `no_answer`) |
| `voice` | `state`, `enabled`, `mode`, `muted`, `deafened`, `me`, `peers` | the voice state changed, or while voice runs a level or a link changed (at most ten a second) |
| `party_invitation` | `invitation` | the leader of a party invited the player |
| `party_invitation_gone` | `id` | an invitation expired, was answered or used up |
| `conversations` | `conversations`, `requests` | after sign-in and after every reconnect |
| `message` | `conversation`, `seq` | a new message; fetch it with `messages` |
| `message_request` | `conversation` | a stranger wrote for the first time |
| `read` | `conversation`, `account`, `seq` | a member read up to `seq` |
| `delivered` | `conversation`, `account`, `seq` | a member's device received a direct conversation up to `seq` |
| `typing` | `conversation`, `account`, `typing` | a member types, or stopped; show it for 6 seconds |
| `message_update` | `conversation`, `seq`, `rev` | a message was edited, unsent or reacted to; fetch `messages` with `changed_after` |
| `conversation` | `id` | a conversation is new, or its members, name, mute or pin changed |
| `conversation_gone` | `id` | the player is no longer in the conversation |
| `screen` | `can_share`, `can_watch`, `hosting`, `offers`, `watching` | share screen changed; while a stream runs its numbers at most twice a second |
| `feed` | | the feed has news; fetch `feed`. The net service sends at most one every two seconds, with one trailing event for news inside that time |

- `client_too_old` ends the event connection for good: the `cloud` state goes to `offline` and stays there, and the net service does not try again until it is started again, so the launcher shows its update screen and does not wait for the connection. The sign-in stays kept. Every command that reaches the cloud afterwards still fails with the `client_too_old` failure (with `minimum`), without a second event. A launcher that does not know the event ignores it, and a failure with an unknown code is a failure like any other.
- `restrictions` lists only the mutes that have not ended or been lifted; a warning limits nothing and is not listed, a new account's limit comes with the `restricted` failure. The net service sends it only when the list changed. A restriction with `until` in the past is over: the launcher drops it at that time without waiting for an event.
- To appeal: a mute is appealed with the `appeal` command. A ban signs the account out, so it is appealed on the account web: the launcher starts `browser_sign_in`, and the cloud's sign-in ends on the page with the statement and the form (after a ban the player proves who they are there, as in [ADR 0015](../docs/adr/0015-moderators-are-accounts.md)).
- `cloud` is `offline`, `connecting` or `online`. `account` is `{id, handle, email}` or null. `pending` is `code` (an e-mail is out), `handle` (the account needs a handle), `browser` (the system browser has the sign-in) or null.
- `friends` carries the whole lists each time, with each friend's presence as in the cloud API. The net service sends at most four of them per second.
- `report` takes the optional `conversation` and `seq` together: it reports that message of the account ([cloud API](cloud-api.md#reporting-a-message)).
- Edits, unsends, reactions, typing and the settings are the [cloud API](cloud-api.md#changes-to-messages) one to one, with its errors. `typing` is fire and forget: the launcher sends it on every key if it likes, and the net service drops all but one `true` in three seconds for a conversation (`false` always goes) and answers `{}`.
- `message` carries no text: the launcher keeps the highest number it has for each conversation and calls `messages` with `after`. The `conversations` event after a reconnect carries every conversation's newest number (`last`), so what arrived while the connection was down is fetched the same way.
- `muted` and `deafened` in the `voice` event say what `mic_mute` and `deafen` set (both false after the service starts, and remembered while it runs). Both silence `me`: its `speaking` is false.
- `voice.state` is `off` (switched off, no party, or the devices could not be opened), `connecting` (switched on in a party: the devices are opening, or members who take part are not reached yet) or `on` (the devices run and at least one member who takes part is reached, or none takes part). `enabled` is whether the player switched voice on, which stays true across parties. `mode` is `push_to_talk` or `voice_activity`. `me` is `{level, speaking}` for the player's own microphone after the gate. `peers` has one entry for each other member of the party: `{account, level, speaking, link}`, with `link` `connecting`, `direct` or `relay` for the voice connection to that member and null when there is none (they have voice off, or have not been reached). `level` is the recent loudness in percent (0 to 100) before the player's volume setting; `speaking` is what a speaking indicator should show. While voice is off, `me` is silent and `peers` is empty. While voice runs, the net service reports levels and links only when they changed.
- The `party` event and the party answers leave out the members' tunnel keys and relays (`endpoint` is `""`, `relay` null), as `session` does; each member's `voice` says who takes part in voice.
- `session` is the cloud's `Session` without `build`, `properties` and the members' `endpoint`, and with one more field per member: `link`, which is `connecting`, `direct` or `relay` for the other players and null for the player.
