# Voice cancels echo with AEC3, ported to Rust, on by default

[ADR 0014](0014-voice-is-an-engine-without-a-transport.md) left echo to push-to-talk and headphones and named echo cancellation as the next step if players ask for an open microphone on speakers. A console-style launcher is played from a couch with the TV's or a soundbar's speakers, so that is the normal case, not the exception: the far member's voice leaves the speakers, enters the microphone, and goes back to the whole party. The engine now cancels it. `Config::echo_cancellation` is on by default, the control protocol has `voice_echo` to switch it off, and the canceller sits first in the sending chain: capture, resample to 48 kHz, **echo cancellation**, RNNoise, gate, Opus.

## The choice: `sonora`, the pure-Rust port of WebRTC's AEC3

[`sonora`](https://crates.io/crates/sonora) 0.2 (BSD-3-Clause, [source](https://github.com/dignifiedquire/sonora)) is WebRTC's audio processing module M145 ported to Rust: AEC3 with its own delay estimator and echo-path change detection, a noise suppressor, AGC2, and SIMD paths (SSE2, AVX2, NEON). It is pure Rust with three small dependencies of its own, so it builds the same on Windows (MSVC and GNU), Linux and macOS with the toolchain the workspace already has, and needs no system package, no CMake step, and no meson or ninja. BSD-3-Clause is already on the allow list of `deny.toml` and is compatible with distributing under MIT OR Apache-2.0 (the notice is kept in the licence listing the launcher ships). It needs Rust 1.91, so the workspace `rust-version` goes from 1.88 to 1.91; CI and the launcher build with stable, which is newer.

What we use of it is the echo canceller. We do not use its noise suppressor (RNNoise stays: its speech probability is the voice activity detector, and it is tuned) and not AGC2. The gain control would be cheap to add, but a microphone whose level the player cannot see moving is a setting of its own, and nobody asked.

### The delay

AEC3 needs the exact signal the speakers play and finds the delay to the microphone itself, up to a few hundred milliseconds. The engine knows the first part exactly, because it writes the playback samples itself, from the mixer, in 20 ms frames. It writes up to 60 ms ahead of the device, though, so handing the reference over at once would add that queue to the delay AEC3 must find, and the queue's length changes with every pump. Instead the reference waits in a small line and is released to the canceller when the device has taken it off its queue (`playback_queued` shrank). What AEC3 still has to find is then the device's hardware buffers, the room, and the capture ring, which is its job, and a fixed start hint of 40 ms. The queue length is counted at 48 kHz whatever the device's rate.

## Considered options

- **`webrtc-audio-processing` (tonarino), the C++ library behind a Rust wrapper** (2.1, BSD-3-Clause style licence): the original AEC3, with years of field use. Rejected as the first choice only for the build: the `bundled` feature needs meson, ninja and a C++ compiler on every runner and every developer's PC, without it the build links the system library (`libwebrtc-audio-processing-dev`), which the release builds do not have and Windows does not have at all. This is the fallback, behind the same two calls (`render`, `capture`) in `echo.rs`, if `sonora` turns out to behave worse than the original on real hardware: it is a one-file swap.
- **`aec3`** (RubyBit, MIT OR BSD-3-Clause): another port of AEC3, built around a node graph with its own scheduler. More than we need to put one filter in a chain.
- **The speex canceller (`speexdsp` bindings)**: older, a simple adaptive filter, no delay estimation, a C dependency. Clearly behind AEC3 for a room with reflections.
- **The operating system's canceller.** Windows: capturing in the communications category turns on the driver's echo cancellation only where the driver provides one, which many do not, and cpal does not expose the category. PipeWire and PulseAudio: `module-echo-cancel` is a module the user loads, not something an application can count on. macOS: the VoiceProcessingIO audio unit does it well, but it is a different audio API from the one cpal uses and also changes the output (ducks other audio, which would duck the game). Three platform back ends with three behaviours, none testable without a device, against one library that is the same everywhere and can be tested offline. Rejected as the primary; if a platform later offers a reliable one, it can come in front of this as a bypass.
- **Nothing, as before.** Rejected by the reason above.

## What follows

- The setting is `Config::echo_cancellation` and `Engine::set_echo_cancellation`; the net service carries `voice_echo { enabled }` ([control protocol](../../spec/control-protocol.md)). An older launcher never sends it and gets the default, on; an older net service that receives it answers it as an unknown command, which a launcher treats as "not supported".
- With headphones there is no echo and AEC3's transparent mode keeps the player's voice as it was; a test checks that a silent reference changes the voice by less than 1.5 dB.
- The cost is a few percent of one core while the engine runs.
- Tests are offline (`crates/voice/tests/echo.rs`): a fixed-seed voice plays into a simulated room (a 40 ms delay and three reflections) and the figure asserted is ERLE, with the canceller alone, with a second voice talking at the same time, and through the whole engine (two engines, Opus in between).
- Not covered by any test, because it needs hardware: how it sounds on real speakers and microphones, with devices whose buffers drift against each other, and on a laptop with the microphone a few centimetres from the speakers. Those are listening tests on real PCs.

## Update (28.10.2026): double talk on real speech, and clock drift

The first double-talk test used two synthetic voices (harmonic tones with formants). It reported only 0.36 correlation between the canceller's output and the player's voice, and the question was whether AEC3 really cuts into a player who talks over the far end, or whether the signals were to blame. `crates/voice/tests/echo_speech.rs` repeats it with speech: four 6-second LibriSpeech clips (CC BY 4.0, attribution in `tests/fixtures/librispeech/README.md`), two speakers, one the far end and one the player, 28 seconds, both at an RMS of 0.1 (so the echo at the microphone is about as loud as the player), in the same simulated room. "Voice kept" is the correlation of the output with the player's voice during double talk, divided by the same with the far end silent, so it measures what the canceller does to the voice beyond what any processing does.

| Extra delay | Clock drift over 28 s | ERLE, far end only | ERLE in the player's pauses | Voice kept |
|---|---|---|---|---|
| 0 ms | 0 ms | 49.2 dB | 20.7 dB | 0.75 |
| 20 ms | 0 ms | 48.6 dB | 19.8 dB | 0.78 |
| 40 ms | 0 ms | 49.2 dB | 20.8 dB | 0.79 |
| 0 ms | 20 ms | 22.4 dB | 11.1 dB | 0.73 |
| 0 ms | 40 ms | 33.7 dB | 5.1 dB | 0.64 |

- **The 0.36 was the signals.** With real speech about three quarters of the voice's correlation survives when the far end talks at the same time, and the echo in the player's pauses is 20 dB down. The synthetic voices are periodic, which is the case an adaptive filter cannot tell apart from an echo of a periodic far end. The synthetic double-talk test stays in `echo.rs` as a stress case with a loose bound.
- **A fixed extra delay of 20 or 40 ms changes nothing.** AEC3's delay estimator finds it.
- **Clock drift is the weak spot.** The drift rows grow the delay by 20 or 40 ms over the 28 seconds, which is 700 to 1400 ppm; two sound cards on one laptop differ by tens to a few hundred ppm, so this is a stress test about ten times harsher than a typical laptop. The filter still takes the echo down (22 and 34 dB with only the far end; 11 and 5 dB while the player talks), and the voice is kept at 0.73 and 0.64, but the residual echo in double talk at the harshest drift is audible. AEC3 has a `has_clock_drift` mode for this which the `sonora` API does not expose.
- **No tuning was possible, and none was needed.** `sonora`'s `AudioProcessing` exposes one AEC3 setting, the transparent mode (counter or classifier). Both gave identical figures on the first speech run, so it never engaged here and it is not exposed by our API. The suppressor, delay estimator and `has_clock_drift` lie in `sonora-aec3`'s `EchoCanceller3Config`, which `sonora` builds itself; reaching them means driving `sonora-aec3` directly (with our own band splitting) or carrying a patched copy of `sonora`. We do neither on this evidence: the voice is kept and the echo is mostly gone.
- **The default stays on.** The remaining risks are the ones the offline tests cannot show: real devices, with their own buffering jitter and drift, and rooms that are not linear. The player has "Echo Cancellation" in the voice settings to turn it off, and the note next to it says when to.
