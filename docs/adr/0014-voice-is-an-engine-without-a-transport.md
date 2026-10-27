# Voice is an engine without a transport

Party voice is the crate `anyplay-voice`: bytes in, bytes out, with the sound hardware behind a trait. It encodes with libopus, suppresses noise with RNNoise, runs its own jitter buffer and mixer, and knows nothing about parties, accounts or tunnels. The net service gives it datagrams from the tunnels and sends the datagrams it returns ([ADR 0010](0010-voice-and-shared-play-go-through-the-tunnels.md)). Push-to-talk is the default, and there was no echo cancellation (since [ADR 0040](0040-voice-cancels-echo-with-aec3-in-pure-rust.md) there is): a player who opens the microphone without headphones is heard by the others as an echo of their own party. The hardware (cpal) is behind the cargo feature `devices`, so builds and tests need no audio headers.

## Considered options

For the codec:

- **libopus through the `opus` crate** (with `opusic-sys`, which builds the bundled C source with CMake): the reference codec with its forward error correction, discontinuous transmission and concealment, which a talk with 30 % loss depends on. The build needs CMake and a C compiler, which both CI runners, and a Windows machine with MinGW or MSVC, have. Chosen.
- **`unsafe-libopus`**, libopus machine-translated to Rust: needs no C toolchain, but it is unsafe code throughout, younger and less used. The choice to revisit if the C build ever costs more than it saves.
- **`audiopus_sys`**, the older binding: not updated since 2021.

For echo:

- **Push-to-talk by default, no cancellation.** A game party sits at a PC with headphones or a pad in hand, and the microphone is closed while no key is held. Open microphone is offered with voice activity detection and a warning.
- **Echo cancellation** (WebRTC's AEC3, or the speex canceller): it needs the exact playback signal and its delay at the microphone, which differs per device and driver, and we found no mature, permissively licensed one in Rust. It is the next step if players ask for open microphone on speakers, and the seam in `AudioIo` already sees both streams.

For noise suppression:

- **`nnnoiseless`**, a pure-Rust port of RNNoise (BSD-3-Clause): about 2 % of a core for the whole engine, and it reports a speech probability that serves as the voice activity detector. Chosen.

For the jitter buffer, resampler and mixer:

- **Our own, a few hundred lines each.** The jitter buffer is built around Opus's forward error correction and the end-of-talk flag, so a general one would have to be bent to fit; the resampler only converts between a device's rate and 48 kHz. Each has tests with a simulated network.
- **Embedding WebRTC's NetEQ, or a second voice stack:** rejected with WebRTC itself in ADR 0010.
