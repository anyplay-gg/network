# Clips

How the launcher has the last seconds of a running game kept and saved as an MP4 file. Version 0, a draft: it changes without notice until version 1. The decision is [ADR 0028](../docs/adr/0028-clips-are-kept-by-the-worker-and-stay-on-the-pc.md).

Clips touch no network, no account and no cloud. The share worker (`anyplay-share`) runs in a mode of its own and the launcher talks to it over the worker's standard input and output.

## Starting

```text
anyplay-share --clips SECONDS [--title TITLE] [--window HWND] [--pid PID]...
```

- `SECONDS` is how long a clip is at least: the launcher offers 15, 30, 60 and 120.
- `--window`, `--pid` and `--title` name the game's window by the rules of [share screen's capture](screen-share.md#capture).
- The launcher starts one worker for a running game when Clips is on, ends it (closes its standard input) when the game ends or Clips is switched off, and starts none when Clips is off. A worker that exits is not restarted for the same game.

## What the worker records

| | |
|---|---|
| Picture | the game's window, scaled to fit 1280 by 720 with black bars, 30 pictures a second (the hardware encoder may lower it) |
| Encoder | H.264 from the first **hardware** encoder Media Foundation lists (on macOS the media engine through VideoToolbox, [ADR 0036](../docs/adr/0036-macos-captures-with-screencapturekit-and-encodes-with-videotoolbox.md)); never the software encoder. Constant bit rate 6000 kbit/s, no B-frames, [as ADR 0027](../docs/adr/0027-video-is-h264-from-the-system.md) |
| Key frames | one every 2 seconds, with the SPS and PPS in front |
| Ring | whole groups of pictures (a key frame and what follows it). The oldest group is dropped as soon as the groups after it cover `SECONDS`, so a clip is at least `SECONDS` long and at most 2 seconds longer, and starts on a key frame. A limit of 160 MiB drops older groups too, down to the newest one |
| Sound | none (version 0); the file has one track |
| A window that does not change | gives its last picture again, so a still scene is still a clip |

Memory is the only storage: nothing is written until a `save`, and the ring ends with the worker.

## Commands

One JSON object per line on standard input, UTF-8. Unknown commands are ignored; closing the input is a `quit`.

| `do` | Fields | Meaning |
|---|---|---|
| `save` | `path` | Write the ring to this file. The worker creates the folder, writes `path.part`, and renames it when the file is whole, so a folder never holds half a clip. The recorder goes on while the file is written. |
| `quit` | | End. |

## Events

One JSON object per line on standard output.

| `event` | Fields | When |
|---|---|---|
| `ready` | `width`, `height`, `fps`, `kbps` | The encoder runs and the ring fills. |
| `saved` | `path`, `seconds`, `bytes` | A `save` finished: the clip's length and the file's size. |
| `save_failed` | `reason` | A `save` did not work: `empty` (no key frame yet, nothing kept) or the error in words. The recorder goes on. |
| `ended` | `reason` | The worker is ending: `no_encoder` (no hardware H.264 encoder, or none that accepts the setup), `no_window` (the game's window was not found), `no_permission` (macOS: Screen Recording is not switched on for the app that started the worker, see [the permission](screen-share.md#screen-recording-permission)), `window_closed`, `encoder_failed: ...`, `quit`. |

Exactly one `saved` or `save_failed` answers each `save`, in the order the files finish. Diagnostics go to standard error and are not part of the protocol.

## The file

MP4 (`isom`, `iso2`, `avc1`, `mp41`), one H.264 video track, samples timed in 1/90000 s from the picture's capture time, the `avcC` box from the first key frame's SPS and PPS, the pictures length-prefixed. The index is at the end of the file (no fast start). Anything that plays H.264 in MP4 plays it.

## Not in version 0

- Sound: the game's sound by process loopback capture and a second track (see [Not in version 0](screen-share.md#not-in-version-0) of share screen, which needs the same).
- Software encoding, 1080p, HDR, a recorder that runs without a game.
- The engine tap as a source (ADR 0023).
