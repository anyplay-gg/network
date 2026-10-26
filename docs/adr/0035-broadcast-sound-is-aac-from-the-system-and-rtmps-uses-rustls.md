# Broadcast sound is AAC from the system, and rtmps uses rustls

Extends [ADR 0027](0027-video-is-h264-from-the-system.md); the wire is in [spec/broadcast.md](../../spec/broadcast.md).

The first broadcast sent video and a track of silence over plain `rtmp://`. Two things were missing: the game's sound, and TLS for services that ask for it (YouTube recommends `rtmps://`).

## Decision

- **The game's sound is the same capture as share screen.** `loopback::LoopbackSource` takes the sound of the game's process tree (WASAPI process loopback), never the microphone and never other programs. The broadcast reuses it as it is; there is no second capture path.
- **AAC comes from the system, as H.264 does.** RTMP and FLV carry AAC, not Opus. On Windows the encoder is the AAC encoder of Media Foundation (`CLSID_AACMFTEncoder`): AAC-LC, 48 kHz, stereo, 128 kbit/s, raw frames. The worker contains no AAC code and distributes no encoder, so it owes no AAC royalty and adds no dependency or licence (the same position as ADR 0027). Windows N editions without the media codecs have no such encoder: there, and on Linux and macOS, the track stays the silence it was, and the log says why.
- **No AAC library.** The usual alternatives were left out on purpose: FDK-AAC has a licence that is not free for distribution, FFmpeg's native AAC encoder means shipping FFmpeg (the ADR 0027 reasoning), and there is no mature pure-Rust AAC encoder. A Linux sound track is a later change when one of these changes or a patent-clean encoder exists.
- **The track keeps the pictures' clock.** Process loopback delivers nothing while the game is quiet and delivers in bursts. A `Timeline` keeps the samples given to the encoder in step with the wall clock the pictures use: silence fills a quiet stretch, a pile-up is cut. A message's time is its frame's place on that line. A failing source or encoder turns the track into silence; it does not end the broadcast.
- **`rtmps://` is rustls, with the system's certificate checks.** The network already uses rustls (ring provider) and `rustls-platform-verifier`; the share worker takes the same, so there is no OpenSSL and no second trust store: the operating system's roots, revocation and date rules decide. Port 443 is the default. The connection state sits in one mutex and the socket is read outside it, so the reader that answers pings and the sender that writes pictures never wait for each other.
- **Failures are tagged so the launcher can say them in plain words.** `tls_untrusted` (not a certificate this PC trusts, or for another server name), `tls_expired` (not valid at this PC's date and time), `tls_failed` (anything else), after `connect_failed:`.

## Consequences

- `rustls` and `rustls-platform-verifier` become dependencies of the share worker (Apache-2.0/MIT/ISC; both were already in the lockfile).
- The Windows CI job runs the AAC encoder test, which skips on a runner without the codec.
- A real stream to Twitch or YouTube is not part of CI; the RTMP and TLS code is tested against servers of our own and, by hand, by an ignored test against ffmpeg's RTMP server.
