# Clips are kept by the share worker and stay on the PC

Extends [ADR 0027](0027-video-is-h264-from-the-system.md). The details are in [the clips spec](../../spec/clips.md).

A player can have the launcher keep the last seconds of their game (15, 30, 60 or 120) and save them as an MP4 file when they press a key or a controller button. The launcher draws the settings, the hotkey and the toast. The question here is who holds the pictures.

**The share worker keeps them, in a mode of its own.** The worker already finds the game's window, captures it with Windows Graphics Capture and encodes H.264 on the GPU through Media Foundation. Clips need exactly that and nothing else: no link, no packetiser, no network. The worker gets a mode, `--clips SECONDS`, in which it records into a ring of encoded pictures and answers JSON lines on its input and output. The ring and the MP4 writer are pure code in the worker's library (`clips::ring`, `clips::mux`) and are tested on every system; only the capture and the encoder are Windows.

Where else it could live, and why not:

- **In the launcher.** The launcher is GPL-2.0 and would need its own capture and encoder, a second copy of what ADR 0027 settled, or a dependency on the worker's crates that the launcher otherwise reaches only through a process boundary. It would also put picture memory (up to 160 MiB) and an encoder into the process that draws the interface.
- **In the net service.** It has no pictures and no reason to have any. A clip needs neither account nor tunnel.
- **In the engine (AnyPS5).** A tap inside the game could carry sound and the engine's own frames, and stays the better source later (ADR 0023 names it). It does not exist yet, it would be a change to the fork for every game, and the window works for all of them now.

The interface is two verbs: `save` and `quit`. The caller picks the file; the worker never decides a name or a folder, and reports `saved` or `save_failed` once per `save`. That is the whole seam, so the launcher does not know about key frames, rings or MP4.

**Hardware encoder or nothing.** A recorder runs as long as the game does. The system's software encoder is fine for a stream a player chose to start and watches; for a background recorder it would take the game's processor on the weak laptops this is meant for. Clips use only a hardware H.264 encoder. Without one the worker ends with `no_encoder` and the launcher says Clips need a graphics card with a video encoder. A clip is 1280 by 720 at 30 pictures a second at 6 Mbit/s, a key frame every 2 seconds: 90 MB for 2 minutes, a limit of 160 MiB on the ring whatever the picture does.

**Video only for now.** Game sound needs a process loopback capture (WASAPI, Windows 10 build 20348 and later) and an AAC or Opus encoder in the MP4. That is the task that also gives share screen its sound (the spec's "Not in version 0"); the file format takes a second track without a change to anything above. Until then a clip has no sound, and the launcher's settings say so.

## Privacy and the line

- **Nothing leaves the PC.** A clip is written to the player's screenshot folder by the worker and goes nowhere else: no upload, no cloud request, no event for the party. The ring is memory only and ends with the worker; nothing is written until the player asks.
- **Nothing runs when Clips is off.** No worker is started, so nothing is captured or encoded. The launcher starts the worker when a game starts with Clips on, and ends it with the game.
- **The game's window only,** by the same rules as share screen (the window the launcher named, else the largest of the game's processes). Not the desktop, not other windows.
- **The line (ADR 0021).** Capture of the player's own game on their own PC; no platform server, code, key or document; no anti-cheat or copy protection touched; no platform trademark in a name (the feature is called Clips). The codecs are the system's, as in ADR 0027, so the patent position is the same and stays the same review item.

## Licences

The new dependency is `mp4` 0.14 (MIT, pure Rust, about 15 million downloads), with `byteorder`, `bytes`, `num-rational`, `serde`, `serde_json` and `thiserror` it already pulls in, all MIT or Apache-2.0. `cargo deny --all-features check` stays green and the policy of `deny.toml` is unchanged. MIT is allowed under both licences of this repository and under the launcher's GPL-2.0. It was taken over writing a muxer because MP4's index boxes are easy to get subtly wrong; the other candidates were `shiguredo_mp4` (Apache-2.0 only, which the GPL-2.0-only launcher cannot take in), `re_mp4` (a reader first) and `muxide` (young). `mp4` has had no release since 2023; it is small and the file format does not move, and a replacement is one module (`clips::mux`).

## Consequences

- `crates/share-worker` gets the module `clips` (`ring`, `mux`, `recorder`), the function `clips_encoder` and the flag `--clips`.
- No change to the share link, the control protocol, the cloud or the net service.
- The launcher carries the same worker it already carries for share screen and starts it with `--clips`. Its release workflow already builds and attests the worker.
