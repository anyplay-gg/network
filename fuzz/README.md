# Fuzz targets

Coverage-guided fuzzing with [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) of the parsers that read another process's bytes. A game process writes the bridge protocol and the launcher writes the control protocol, so nothing either sends may crash the net service.

| Target | What it reads | What must hold |
|---|---|---|
| `bridge_message` | one frame payload; the first two bytes pick the kind | read or refused; a message read is written back so that it reads the same, with the same kind |
| `bridge_stream` | the bytes of a connection, cut into pieces | every frame stays within the size limit and encodes back to itself; its message reads back the same |
| `traffic_header` | a game datagram or the start of a game stream | the header is exactly the first eight bytes; the rest is left as it came |
| `voice_frame` | a voice datagram from another member of a party | read and written back to the same bytes, or refused for exactly the reason its length, version, flags or payload give |
| `control_line` | one line between the launcher and the net service | text that is not UTF-8 is refused; a command or output read is written back so that it reads the same |
| `share_video_datagram` | a video datagram of share play from another member | read and written back to the same bytes, or refused for exactly the reason its length, version, type, flags or group layout give |
| `share_audio_datagram` | an audio datagram of share play | read and written back to the same bytes, or refused for exactly its reason |
| `share_pad_datagram` | a pad datagram of share play from the guest, and a run of them through the host's gate | read and written back to the same bytes, or refused; no state with a reserved button bit gets through, none in `watch`, and no more than 30 in a burst and 300 a second |
| `share_reassembler` | arbitrary datagrams with time passing; or a packetised frame with up to its parity of packets dropped | never more than 4 frames and 16 MiB in reassembly; a recoverable frame comes back byte for byte |
| `share_control_line` | one line of share play's control stream | over 4 KiB, a second line or non-UTF-8 is refused; a message read is written back so that it reads the same |

The fixed-seed tests next to each parser cover the same ground on every `cargo test`; these targets search further.

## Run

cargo-fuzz needs a nightly toolchain and Linux or macOS:

```sh
cargo install cargo-fuzz --version 0.13.2 --locked
cargo +nightly fuzz run bridge_stream -- -max_total_time=300
```

An input that breaks a target is saved under `fuzz/artifacts/<target>/`. Replay it with `cargo +nightly fuzz run <target> <file>`, then add it to the parser's tests as a fixed case before fixing the parser.

## In CI

[`.github/workflows/fuzz.yml`](../.github/workflows/fuzz.yml) runs each target for a minute on pull requests that touch the parsers, and for a quarter of an hour every night. The corpus grows from run to run in the Actions cache. When a target fails, the input is attached to the run as the artifact `crash-<target>`.
