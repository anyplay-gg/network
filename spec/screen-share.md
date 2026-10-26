# Share screen

How a player streams the game they are playing to the members of their party, who watch it in their launcher. Watch only: the picture and the sound go one way and nothing comes back. Version 0, a draft: it changes without notice until version 1.

The decision is [ADR 0023](../docs/adr/0023-share-screen-is-watch-only-and-asks-the-cloud-for-nothing.md). It is the watch-only part of [share play](share-play.md) and uses its media path unchanged: the [video and audio datagrams](share-play.md#video-datagram), the [loss protection](share-play.md#packetising-and-loss-protection), the [codecs](share-play.md#codecs), the [rate and its ladder](share-play.md#congestion-control-and-the-rate), the [share link](share-play.md#the-share-link) and the share worker. This file lists only what differs: who connects to whom, the messages, the control protocol and the stops.

## What it is

The **streamer** runs a game in AnyPS5 and presses **Share screen**. Every other member of the party is offered the stream. A member who presses **Watch** sees the game's picture and hears its sound, in a window of the share worker, until they press **Leave** or the streamer stops.

| A viewer can | A viewer cannot |
|---|---|
| See and hear what the game shows and plays | See the streamer's desktop, other windows, notifications or the launcher |
| Leave at any moment, and watch again while the stream runs | Send any input: no pad, keyboard, mouse, text or clipboard message exists on this connection |
| Talk in the party's voice as before | Start, stop or change anything on the streamer's PC; read any file; get game data, saves, trophies or tokens |

- Only the streamer starts and stops a stream. Nothing starts without a person pressing a button on each side, and nothing is remembered: the next stream is offered again.
- One stream per player at a time. Up to **4 members** watch one stream at once. A player watches one stream at a time.
- It works only between members of the same party, and only while the streamer's game runs.
- The cloud never sees pictures, sound or even that a stream runs (see [the cloud](#the-cloud)). The viewer has no record or screenshot button; their operating system can still record their own screen, as with every such service.

## Parts

| Part | Does | Does not |
|---|---|---|
| **Share worker** (`anyplay-share`) | Streamer: captures the game's window (or the test picture), encodes, packetises, adds loss protection. Viewer: reassembles, decodes, shows a window, plays the sound. | Touch the network or the account |
| **Net service** | Holds the connections, checks who connects, starts and ends the worker, measures the path, sets the worker's rate, forwards datagrams, tells the launcher. | Look into pictures or sound |
| **Launcher** | The Share screen, Watch, Leave and Stop buttons; the streamer's indicator; the viewer's panel; downloading and verifying the worker. | Decode video |

The Rust reference of the messages is `crates/share` (`link`), of the worker `crates/share-worker` (the program `anyplay-share`), and of the connections `crates/net` (`screen`, `worker_process`). The net service starts the worker as a child process for the stream and ends it with the stream, handing it the share link's address and token in `ANYPLAY_SHARE_ENDPOINT` and `ANYPLAY_SHARE_TOKEN`; the launcher tells it where the verified worker is (`screen_worker`). A worker that exits ends the stream on that side and the launcher gets a `notice`.

## Connections

One QUIC connection per streamer-viewer pair, with the ALPN `anyplay/screen/0` on the tunnels' iroh endpoint, so direct or relayed paths, hole punching, tickets and the relay-only setting apply as for tunnels and voice ([ADR 0017](../docs/adr/0017-voice-has-a-connection-of-its-own.md)).

1. The party names each member's tunnel key (`endpoint`) and relay in the cloud's `Party` ([cloud API](cloud-api.md#parties)). That list is the only source of who may connect.
2. **The streamer dials, the viewer accepts.** While a stream runs, the streamer's net service dials every other member who has an endpoint, with the key and relay the party names, and repeats a refused or ended dial after 0.5 s, doubling to at most 30 s (as voice does). Two members who both stream hold two connections between them, one in each direction; a connection is the viewer side for whoever accepted it.
3. A connection counts only when the remote endpoint, which the TLS handshake proves, is the endpoint of a current member other than the player. A dial whose answer comes from another key than the member's fails. Anything else that arrives is closed at once with code 1 and never reaches the worker or the launcher. A second connection from the same streamer replaces the first (code 4).
4. When the party's member list changes (a copy with a higher `revision`), connections to endpoints that are no longer a member's, or that belong to another account than before, are closed with code 5.
5. The streamer's net service takes at most 4 watchers. A fifth `watch` gets `stop` with the reason `full`; the viewer stays an offer.

Close codes: 0 normal end, 1 not in the party, 2 the streamer stopped, 4 replaced, 5 left the party or was removed, 6 protocol error, 8 timed out.

## Control stream

The dialing side (the streamer) opens one bidirectional stream right after the handshake. Messages are those of [share play's control stream](share-play.md#control-stream): one JSON object per line, UTF-8, at most 4 KiB, unknown messages and fields ignored, at most 50 messages a second from the viewer (more closes the connection with code 6). In this file:

| `msg` | From | Fields | Meaning |
|---|---|---|---|
| `hello` | streamer | `version`, `role` (`host`), `title`, `encode` | Opens the stream and is the offer: the title of the game and what the streamer can encode. |
| `hello` | viewer | `version`, `role` (`guest`), `decode` | Answer: what the viewer can decode. Sent right away; it is not a request to watch. |
| `watch` | viewer | | Begin sending to me. |
| `unwatch` | viewer | | Stop sending to me; the offer stays. |
| `config` | streamer | `epoch`, `codec`, `width`, `height`, `fps`, `kbps`, `mode` (`watch`) | A new setup, sent after `watch` and on every change of codec, size or rate step. The first picture is a key frame. |
| `report`, `key`, `ping`, `pong` | as in share play | | The rate controller's inputs, key frame requests, clock and round trip. |
| `stop` | streamer | `reason`: `ended`, `game_ended`, `full`, `no_encoder` | The streamer is ending the stream, or refuses this viewer. |

Role names stay `host` and `guest` on the wire, as in share play; this file says streamer and viewer.

## Capture

On Linux the worker asks the desktop portal's ScreenCast for one window: the player picks it in the system's dialog each time a stream starts, no choice is remembered, and the portal hands the worker a PipeWire stream of that window alone. The portal cannot be told which window is the game's, so `window` and `pids` are not used there. Cancelling the dialog ends the stream before it starts, and so does picking a screen: some pickers offer screens whatever is asked for, and only a window is shared. A dialog left open for 2 minutes is given up the same way. The worker takes at most 30 pictures a second from the compositor, what its software encoder sends. Until the window has drawn its first picture the worker keeps taking the net service's messages, so a stop gets through.

On Windows the streamer's worker takes the picture of one window with Windows Graphics Capture. Which window is decided in this order, and a window of a process the launcher did not start is never taken while it has named some:

1. `window`, the window the launcher found for the game, if it still exists, can be captured, is at least 200 by 150 and belongs to one of `pids` (a handle can be reused).
2. The largest window of one of `pids` that is visible, not a tool window and at least 200 by 150. The worker looks for up to 8 seconds, because a game that has just started has no window yet. `pids` are the game's process and what it started, as the launcher's job object lists them.
3. Only when the launcher named neither: a window whose title contains the game's title. This is a guess and the launcher always names the game, so it is the fallback for tools and tests.

A window that does not change (or is minimised) gives its last picture again at the stream's rate, so the viewer sees a live stream. The stream's size is the rate ladder's, not the window's: a picture of another size or shape is scaled to fit with black bars, so a resize of the window never restarts the stream. When the window closes the capture ends and so does the stream. The cursor and the capture border are not drawn.

## Starting, watching, stopping

1. **Start.** The player has a game running (`playing` set, a `game_start` came before), is signed in with tunnels up and in a party with at least one other member, and the worker is there with an encoder. The launcher sends `screen_start`. The net service answers `ok` at once; the `screen` event shows `hosting` with phase `connecting`. It begins to dial the members.
2. **Offer.** A member's net service accepts the connection, reads the streamer's `hello`, answers its own, and shows the stream in `offers` of the `screen` event. Nothing is decoded and no worker runs yet.
3. **Watch.** The member presses Watch: `screen_watch {account}`. Their net service starts the worker as viewer and sends `watch`. The streamer's net service starts the worker as streamer if it is not running (the first watcher starts capture and encoding; with nobody watching, nothing is captured or encoded), picks the codec as share play does, sends `config`, and the first key frame follows. The viewer's `screen` event moves `watching` from `connecting` to `live` with the first picture shown.
4. **Leave.** `screen_leave`, the viewer closing the window, or the viewer's worker exiting: the viewer sends `unwatch`, the stream returns to an offer. When the last watcher has left, the streamer's worker is ended; a new `watch` starts it again.
5. **Stop.** The streamer presses Stop (`screen_stop`): `stop {reason: ended}`, the connections close with code 2, the worker ends.

Stopping is judged by what the viewer sees: the picture stops within 250 ms of the streamer's action, and the streamer's capture ends within 2 seconds.

| Who and how | What happens |
|---|---|
| The streamer presses **Stop** (button in the party page or the indicator) | As step 5. |
| The streamer's game ends (`game_end`, or `playing` set to null) | `stop {reason: game_ended}`, as step 5. |
| The streamer's launcher closes or the net service ends | The connections close (code 0); viewers see the stream end. |
| A viewer leaves the party or is removed, or the streamer is | The connections to them close with code 5; the party event is the trigger. |
| Either side has blocked the other | The net service does not dial a member the player has blocked, and closes a connection from one with code 5. |
| The connection stalls | After 5 s without a datagram the viewer shows that it waits (`stalled`); after 60 s the viewer leaves and the offer goes. |
| The streamer's worker crashes | The streamer's stream ends, the launcher gets a `notice`. The game keeps running; nothing in it waits for the worker. |

The streamer must always have a visible sign that a stream runs and a way to stop it: the launcher keeps the indicator (who watches, the path in words, **Stop**) on screen while `hosting` is set, also from other pages.

## Control protocol additions

For the [control protocol](control-protocol.md#share-screen). The Rust reference is `crates/control` (`ScreenStart`, `Event::Screen`).

| `do` | Fields | `ok` | Errors |
|---|---|---|---|
| `screen_start` | `source`: `game` (default) or `test_pattern`; `window`, `pids`: where the game is (see [Capture](#capture)) | `{}` | `no_game`, `not_signed_in`, `not_in_party`, `no_encoder` (no worker, or it cannot capture or encode), `busy` (already streaming) |
| `screen_stop` | | `{}` | |
| `screen_watch` | `account` | `{}` | `not_found` (that member offers no stream), `busy` (already watching), `no_decoder` |
| `screen_leave` | | `{}` | |
| `screen_worker` | `path` | `{encoders, decoders, capture, libraries}` | `not_found` (no file), `bad_request` (not a worker that speaks this protocol) |

| `event` | Fields | When |
|---|---|---|
| `screen` | `can_share`, `can_watch`, `hosting`, `offers`, `watching` | something changed; while a stream runs its numbers at most twice a second |

- `hosting` is null or `{phase, title, codec, width, height, fps, kbps, viewers}` with one entry for each other member the stream reaches: `{account, watching, link, rtt_ms, loss_percent}`. `link` is `connecting`, `direct` or `relay`.
- `offers` has `{account, title, link}` for each member whose stream the player may watch and is not watching.
- `watching` is null or `{account, title, phase, link, rtt_ms, width, height, fps, kbps, shown_fps}`. `phase` is `connecting`, `live` or `stalled` on both sides.
- `can_share` is true when a stream could start now (see Start). `can_watch` is true when the worker can decode and show. Both are false until a worker is installed, so a launcher with neither and nothing to show draws nothing.
- `libraries` in the `screen_worker` answer names each codec library the worker loads while it runs instead of carrying it (Cisco's OpenH264 on Linux), `{name, state, detail}`, and is absent where it loads none (Windows). `state` is `ready`, `off` (the launcher started the net service with `ANYPLAY_OPENH264=off`, the player's switch), `missing` (not installed), `refused` (a file that is not a known build; `detail` names it and its SHA-256) or `failed` (a file that could not be read, or a known build that would not load). The launcher shows it beside its switch for the library.
- The launcher says the path in words from `link` and `rtt_ms`: "Direct, 24 ms", "Through a relay, 80 ms".
- The launcher never sees tunnel keys, rings or datagrams.

## The cloud

None of it. Share screen adds no request, no state and no event to the [cloud API](cloud-api.md). The party's member list is read as voice reads it. A later presence flag ("is streaming") and the operator's switch (`share.screen` in `GET /v1/info`) can be added without any change to this file's wire formats.

## Moderation and privacy

- Party only: no link, no public stream, no spectator from outside the party. A report names the account; the cloud holds no video, so there is nothing for a moderator to view.
- The picture comes from the game's window only. On Windows it is Windows Graphics Capture of that window; on Linux the portal's window picker, which the player confirms each time (see Capture). The desktop and other windows are not captured.
- A viewer's decoder runs in the worker, a process without account or file access of its own, and only after the viewer chose Watch; the parsers and the reassembler are fuzzed ([fuzz](../fuzz/README.md)).
- Relay-only ("Hide Your IP Address") applies: with either side set, the stream goes through a relay and the launcher says so ("Through a relay: up to 720p60").

## The line check

Against [ADR 0021](../docs/adr/0021-the-line-is-drawn-at-the-law.md): no platform server, code, key, firmware or document is used and none of the formats is a platform's; no token is issued; the game runs only on the streamer's PC, on the streamer's copy, and viewers get a picture and sound as if they watched over the shoulder, never the game or its data; nothing touches anti-cheat or copy protection; no platform trademark is in a name (the feature is called Share screen). The codecs' patent question is the one of [share play](share-play.md#codecs) and stays a review item.

## Not in version 0

- Input of any kind from a viewer (that is share play's `hand_over` and `together`, behind `share.enabled`).
- Viewers outside the party, a public link, recording or clips.
- Several rates (one per viewer): the stream follows the weakest watcher.
- Game sound on Linux, at either end. On Windows the streamer's worker captures the game's process tree (WASAPI process loopback, Windows 10 build 20348 and later), sends it as Opus in the [audio datagram](share-play.md#audio-datagram) and the viewer's worker plays it; a viewer whose worker plays no sound says "no sound" in its window title. Keeping sound and picture within 40 ms of each other.
- The engine tap as a source (see ADR 0023), HDR, 4K, more than two channels of sound.
- A "is streaming" mark in the friends list.
