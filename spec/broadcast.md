# Broadcast

How the launcher has the share worker publish a running game's window live to an RTMP server: Twitch, YouTube or any other. Version 0, a draft: it changes without notice until version 1.

Broadcast touches no AnyPlay account and no AnyPlay cloud. The share worker (`anyplay-share`) runs in a mode of its own and the launcher talks to it over the worker's standard input and output. The stream key is a secret: it goes to the worker on standard input only, never on the command line, never into a log.

## Starting

```text
anyplay-share --broadcast [--width W] [--height H] [--fps F] [--kbps K]
              [--title TITLE] [--window HWND] [--pid PID]...
```

The first line on standard input must be

```json
{"do":"publish","url":"rtmp://live.twitch.tv/app","key":"..."}
```

`--window`, `--pid` and `--title` name the game's window by the rules of [share screen's capture](screen-share.md#capture). The launcher starts one worker for a broadcast and ends it (closes its standard input) when the player stops the broadcast or the game ends.

## What the worker sends

| | |
|---|---|
| Picture | the game's window, scaled to the size asked for (default 1280 by 720, 30 pictures a second), lowered to what the encoder gives |
| Encoder | H.264 from the first hardware encoder the system lists, else the system's software encoder (Windows) or OpenH264 (Linux); constant bit rate (`--kbps`, default 4500); no B-frames |
| Key frames | one every 2 seconds, with the SPS and PPS |
| Sound | AAC-LC, 48 kHz stereo, **silence** (version 0). Some services show "no audio" warnings less often with a track than without one |
| Protocol | RTMP over TCP (`rtmp://`), plain handshake, FLV bodies; `connect`, `releaseStream`, `FCPublish`, `createStream`, `publish`, then `@setDataFrame`/`onMetaData`, the AVC and AAC configuration, then pictures. Pings are answered |
| A connection that cannot keep up | pictures are dropped up to the next key frame; `stats.dropped` counts them |

## Commands

| `do` | Fields | Meaning |
|---|---|---|
| `publish` | `url`, `key` | The first line. |
| `quit` | | End (closing the input is a quit). |

## Events

| `event` | Fields | When |
|---|---|---|
| `connecting` | | The server is being reached. |
| `live` | `width`, `height`, `fps`, `kbps` | The server accepted the stream. |
| `stats` | `seconds`, `kbps`, `dropped` | Every 2 seconds while live. |
| `ended` | `reason` | `no_encoder`, `no_window`, `window_closed`, `connect_failed: WORDS` (the address is wrong, the server was not found or did not answer, or refused the key), `disconnected: WORDS`, `encoder_failed: ...`, `quit`. |

## Not in version 0

- The game's sound (process loopback capture and an AAC encoder).
- `rtmps://` (TLS). Twitch and YouTube both take plain `rtmp://`.
- Linux capture: the portal's window picker needs the player's confirmation each time, so the worker answers `ended` with `no_window` or `no_encoder` there.
