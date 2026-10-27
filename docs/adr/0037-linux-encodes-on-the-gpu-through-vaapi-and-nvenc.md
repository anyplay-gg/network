# Linux encodes on the GPU through VA-API and NVENC, loaded from the system

Extends [ADR 0027](0027-video-is-h264-from-the-system.md) and [ADR 0028](0028-clips-are-kept-by-the-worker-and-stay-on-the-pc.md). ADR 0027 left Linux with Cisco's OpenH264 in software, held to 720p30, and named VA-API as the plan for hardware. This settles how the hardware is reached.

**The worker opens the GPU's encoder through the system's own libraries while it runs, behind the encoder trait it already has.** Two backends, tried in this order, in `crates/share-worker/src/hw`:

- **NVENC** (NVIDIA): `libnvidia-encode.so.1` and `libcuda.so.1`, which the proprietary driver installs. The API is version 11.1, so drivers from the 470 series on work.
- **VA-API** (Intel and AMD): `libva.so.2` and `libva-drm.so.2`, with the driver of Mesa or Intel (`radeonsi`, `iHD`, `i965`). The first render node with an H.264 encoder is used.

Both are opened with `dlopen` (the `libloading` crate, ISC). Nothing is linked at build time, so the worker builds without any GPU header and starts on a PC without any of the libraries. Nothing of a codec is vendored or shipped: the encoder is the driver's, as Media Foundation's hardware transforms are on Windows.

- **Probe at start.** A backend is available when it can open an encoder; `capabilities()` does exactly that once with a small setup and drops it. If one works, the worker announces `h264` as hardware (1080p60) before the software entry, and the net service prefers it. The reasons for a miss go to the log in one line.
- **Fall back, never fail.** When a stream starts, the hardware backend that probed is tried at 1080p60; if it refuses, OpenH264 is used at 720p30 as before. A PC with a GPU and without OpenH264 shares; one with OpenH264 and without a GPU behaves as it did.
- **Settings** are the Windows ones: constant bit rate, no B-frames, one IDR on request and every 600 frames, SPS and PPS with every IDR, BT.709 limited range. The rate follows the controller through the backend's own reconfiguration.
- **Switch.** `ANYPLAY_HWENC` is `auto` (default), `off`, `nvenc` or `vaapi`. A name that matches no backend turns the hardware off rather than picking another GPU.
- **Clips on Linux** use the same backends (hardware or nothing, ADR 0028); without one the worker ends with `no_encoder` as on Windows. Capture is the portal's window dialog, so a clip recorder asks the player to pick the game's window each time a game starts with Clips on.
- **Decoding** stays OpenH264 on Linux; hardware decoding is a later change behind the decoder trait.

## Licences and patents

No new codec code and no new patent position beyond ADR 0027's: the player's driver licenses its encoder, we distribute none. The new dependency is `libloading` 0.8 (ISC, already in the lock file through other crates). The FFI declarations are written by hand from the public headers of libva (MIT) and of NVIDIA's Video Codec SDK as published in FFmpeg's nv-codec-headers (MIT); no header is copied into the tree as a file, and `cargo deny` is unchanged. Some distributions ship Mesa without H.264 encoding for patent reasons; there VA-API finds no encoder and the worker uses OpenH264.

## Consequences

- `share-worker` gets the module `hw` (`mod`, `h264bits`, `nvenc`, `vaapi`); `encoder_for`, `clips_encoder` and `capabilities()` use it on Linux.
- The share link, the control protocol and the net service do not change: `CodecCaps.hardware` already exists.
- The tests use fake backends everywhere; each backend also has a round trip that skips without a GPU (CI's runners have none) and fails instead when `ANYPLAY_REQUIRE_HW` is set, for a machine that has one. Real encoding on real GPUs is not tested by CI.

## Considered options

- **FFmpeg's LGPL libavcodec** (the earlier plan, ADR 0027): reaches the same GPUs, but is a large library to build, update and answer for. Two small backends behind our own trait are less to carry.
- **GStreamer:** a plugin system the player may or may not have, with versions to match.
- **Linking `libva` at build time:** the worker would not start on a PC without it.
