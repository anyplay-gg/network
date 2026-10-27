# macOS captures with ScreenCaptureKit and encodes with VideoToolbox

Extends [ADR 0027](0027-video-is-h264-from-the-system.md) (video is H.264 from the system) and [ADR 0019](0019-share-play-streams-through-a-worker.md) (the worker). It settles how the share worker captures the game and encodes its picture on a Mac, so that clips, broadcast and the streamer's side of share screen work there as they do on Windows.

**On macOS the picture comes from ScreenCaptureKit, the video is H.264 from VideoToolbox, and the worker contains no codec code.** Both are frameworks of the operating system. Apple silicon is the target (the media engine encodes H.264 in hardware); an Intel Mac with macOS 12.3 or later gets Apple's software encoder under the same rules as any software encoder (720p30).

- **Capture.** One window, found by the game's processes by the rule every system shares (`pick`): the window the launcher named, else the largest ordinary window of the game's processes, else (without processes) a title match. ScreenCaptureKit's `SCContentFilter` for a single desktop-independent window shows nothing else on the screen. BGRA frames, at most 1080p in pixels, at most 60 a second; the newest picture is kept and a still window gives its last picture again, as on Windows.
- **Encoder.** A VideoToolbox compression session: low-latency rate control, real-time, no B-frames, High profile (Main where refused), a key frame on request and every 600 frames, SDR BT.709 limited range. A picture goes in as NV12 made by the same `pixels` code as on Windows. VideoToolbox gives length-prefixed NAL units; the encoder rewrites them to Annex B and puts the SPS and PPS in front of every key frame. Clips use the media engine only; a stream or broadcast falls back to the software encoder.
- **Game sound.** A second ScreenCaptureKit stream with audio only, filtered to the applications that own the game's processes, 48 kHz stereo float, handed to the same Opus path as on Windows. Where the game's processes are no application the system lists, the stream has no sound and says so on standard error. Clips and broadcast carry no sound on any system yet.
- **No viewer yet.** The Mac worker reports no decoder, so a Mac streams to others but cannot watch (`can_watch` stays false); the viewer's window and a VideoToolbox decoder are a later change that does not touch the wire.
- **Bindings.** The `objc2` family (`objc2`, `objc2-screen-capture-kit`, `objc2-video-toolbox`, `objc2-core-media`, `objc2-core-video`, `objc2-core-graphics`, `objc2-core-audio-types`, `objc2-core-foundation`, `objc2-foundation`, `block2`, `dispatch2`), all `Zlib OR Apache-2.0 OR MIT`, maintained by the same team and already in the lock file through `cpal`. `deny.toml` is unchanged.

## The permission

macOS keeps a switch for screen capture in System Settings, Privacy & Security, Screen Recording, and shows a program nothing of other programs' windows until it is on. The permission belongs to the app that started the worker (the launcher), not to the worker's file. The worker never prompts on its own:

- `anyplay-share --screen-access` prints `{"screen_access":"granted"|"denied"|"not_needed"}` and does not prompt.
- `anyplay-share --request-screen-access` asks the system to show its prompt (it shows it once per app) and prints the same.
- `anyplay-share --open-screen-settings` opens the Screen Recording page of System Settings.
- Clips, broadcast and a stream end with `no_permission` while it is off. The launcher says in plain words what to do and opens the page.

macOS may ask the player to quit and reopen the app after switching the permission on; the launcher says so.

## Considered options

- **AVFoundation's `AVCaptureScreenInput`** is deprecated for this and cannot capture one window.
- **`CGWindowListCreateImage`** is unavailable from macOS 15 and polls instead of streaming.
- **VideoToolbox through its own C declarations** would avoid the generated bindings, at the cost of unchecked signatures; the bindings give typed ones.
- **ScreenCaptureKit's own recording output (`SCRecordingOutput`)** writes a file and bypasses the ring that clips need.

## Consequences

- `crates/share-worker` gets a macOS module `mac` (permission, ScreenCaptureKit, VideoToolbox), the cross-platform `access` (the permission's words and commands) and `pick` (which window), and the macOS CI jobs build and test it. Tests that need a screen, the permission or the media engine skip where the runner has none.
- Nothing on a Mac has been run against a real game by the author of this change; the first run on a real Mac is the proof.
