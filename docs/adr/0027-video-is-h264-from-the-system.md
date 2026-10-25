# Video is H.264 from the system, and the worker ships no codec

Extends [ADR 0019](0019-share-play-streams-through-a-worker.md) and [ADR 0023](0023-share-screen-is-watch-only-and-asks-the-cloud-for-nothing.md). It settles which codec the first real stream uses on Windows and where it comes from; [share play's codec section](../../spec/share-play.md#codecs) has the details.

ADR 0019 planned AV1 where both sides can, H.264 as the baseline, and a software H.264 from Cisco's OpenH264 binary, reached through the LGPL build of FFmpeg for the hardware encoders. Share screen needs one working path first, on the system most players have. **On Windows the video is H.264, encoded and decoded by the system's own Media Foundation transforms, and the worker contains no codec code at all.**

- **Encoder.** The first hardware H.264 encoder the system lists (the GPU drivers register NVENC, AMF and Quick Sync as Media Foundation transforms), else the system's own software encoder. Low-latency mode, constant bit rate, no B-frames, one IDR on request and a safety IDR every 600 frames, High profile (Main or Baseline where the encoder refuses it), SDR BT.709 in limited range. The software encoder is held to 720p30 and a hardware one to 1080p60, so that the game keeps its processor; the worker lowers the net service's setup to that. The rate follows the controller by changing the encoder's mean bit rate while it runs.
- **Decoder.** The system's H.264 decoder in its low-latency mode, in software for now (hardware decoding through a Direct3D device is a later change that does not touch the wire).
- **Colour and size.** The captured window (BGRA) is converted to NV12 and scaled to fit the stream's size with its shape kept and black bars (`anyplay_share::pixels`, pure and tested everywhere). The stream's size follows the rate ladder, never the window, so resizing the game's window does not restart a stream.
- **No HEVC, and AV1 later.** HEVC stays out (ADR 0019). AV1 stays the preferred codec in `pick_codec`; the system's AV1 support is a hardware encoder on some GPUs and a decoder from the Store, so it is no baseline. It is added behind the same two traits when a measurement says it pays.
- **No OpenH264, no FFmpeg, on Windows.** The system's encoder is itself the software fallback, so the case OpenH264 was planned for does not arise there. Both stay the plan for Linux (OpenH264 loaded from Cisco's binary, VA-API for hardware), where the system offers nothing of the kind.

## Licences and patents

- **Code.** No codec crate is added. The new dependencies of the worker are `windows` (MIT or Apache-2.0, Microsoft), `windows-capture` (MIT) and `minifb` (MIT or Apache-2.0), with what they pull in; `cargo deny --all-features check` stays green and the policy of `deny.toml` is unchanged. Nothing is statically linked from a codec project; Media Foundation, the GPU vendors' transforms and the decoder are the system's and the driver's.
- **Patents.** Using the system's codec is the case the codec pools have in mind for operating systems: Microsoft licenses the codecs it ships and the GPU vendors license theirs, and the player's use of them to show their own game to friends is personal use. We distribute no encoder or decoder, so we owe no royalty on one. This is the cleanest position the feature can have and it is not a legal opinion: the review item in ADR 0023 stays open until a lawyer has read it, and it is the same for every video feature.
- **Editions without it.** Windows N and KN editions ship without Media Foundation's codecs. There the worker finds no encoder or decoder, reports none in `--capabilities`, `can_share` and `can_watch` stay false, and the launcher shows nothing. We do not ship a replacement.

## Consequences

- `crates/share-worker` gets a Windows module `mf` (encoder, decoder, probing), a module `capture` (Windows Graphics Capture of one window, found by the game's processes), a module `window` (the viewer's window), and the trait `VideoEncoder` gains `set_bitrate`; `CodecError` gains `Pending` (a codec with a pipeline has no output for this input yet) and `Failed`.
- The test codec stays, only for streams whose source is the test pattern: the net service refuses to pick it for a game, so a worker without a real codec cannot stream a player's game in a lossless stream of hundreds of megabits.
- A round trip through the real encoder and decoder runs in the Windows CI job and skips where the runner has none.
- Game sound is not carried yet (the viewer's window says "no sound"); see the spec's "Not in version 0".

## Considered options

- **OpenH264 first (Cisco's binary):** software only, 720p30 at best, a download and a licence dance, and it exists on Windows already as the system's software encoder.
- **FFmpeg (LGPL build) for hardware encoders:** reaches the same GPUs but ships a large library we would have to build, update and be responsible for; Media Foundation is the same hardware path without that.
- **Direct NVENC, AMF and Quick Sync calls:** the lowest latency and the most control, three code paths to test on hardware we do not all have. Possible later behind the same trait where a measurement asks for it.
- **AV1 first:** the best picture per bit, but hardware encoders on a minority of PCs and no software encoder fast enough for a game running beside it.
