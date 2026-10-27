# AnyPlay Network

The network behind [AnyPlay](https://anyplaynet.com): accounts, friends, parties, voice, screen sharing and encrypted connections between players over the internet. Players open no ports and need no shared LAN. The network never contacts a platform holder's servers.

This repository holds the net service that runs on each player's PC, the protocols it speaks and the design behind them. It is built for games that run on [AnyPS5](https://github.com/boykopovar/AnyPS5), a compatibility layer (not an emulator) by boykopovar and contributors. You play only games you own.

## How the parts fit

| Part | Where | What it does |
|---|---|---|
| Net service | this repository | Runs beside the launcher on each PC. Keeps the account, the connection to the cloud, the tunnels, party voice and screen sharing. |
| [Launcher](https://github.com/anyplaynet/launcher) | its own repository | Starts the net service and shows the online area: friends, messages, parties, share screen. |
| [Cloud](https://github.com/anyplaynet/cloud) | its own repository | Accounts, friends, presence, sessions, invitations, moderation and relays. Runs at [anyplaynet.com](https://anyplaynet.com). |
| [Bridge](https://github.com/anyplaynet/AnyPS5) | AnyPlay's fork of AnyPS5 | A small layer inside AnyPS5's system libraries that hands a game's sign-in, session and web API calls to the net service. Off unless the launcher switches it on for one game start. |

AnyPS5 already replaces a console's system libraries with its own code, so the line between a game and the network is a function call, not traffic on a wire. The network plugs in at that call:

```text
 Player A's PC                                    Player B's PC
 ┌─────────────────────────┐                      ┌─────────────────────────┐
 │ game                    │                      │ game                    │
 │  └ AnyPS5 libraries     │                      │  └ AnyPS5 libraries     │
 │     └ bridge            │                      │     └ bridge            │
 │         │ loopback      │                      │         │ loopback      │
 │ launcher ─ net service ◄┼──── tunnel (QUIC) ───┼► net service ─ launcher │
 └────────────┬────────────┘   direct or relay    └────────────┬────────────┘
              │                                                │
              └─────────── HTTPS requests, one WebSocket ──────┘
                                       │
                                     cloud
                 accounts · friends · presence · sessions · invitations
                          moderation · relay tickets · relays
```

## Use it

Players get the network through the [AnyPlay Launcher](https://github.com/anyplaynet/launcher), which starts the net service itself. To measure a tunnel between two networks, for example your home and a phone's hotspot:

```sh
cargo run --release -p anyplay-probe -- listen          # on one side; prints a ticket
cargo run --release -p anyplay-probe -- connect <ticket> # on the other
```

It reports whether the tunnel went direct or through a relay, the round trip, loss, jitter and throughput. Add `--relay-only` on either side to measure the relayed path. `listen --key <file>` keeps the listener's identity in a file (created with mode 0600 when missing), so the ticket stays the same when the listener restarts; without it every start makes a new ticket. The probe uses the iroh project's public relays. Only probe packets cross them.

## What's inside

| Path | What it is |
|---|---|
| `crates/net` | `anyplay-net`, the net service on each player's PC: account, cloud connection, bridge server, tunnels, voice, screen sharing and share play |
| `crates/bridge-protocol` | the messages between the bridge and the net service |
| `crates/control` | the messages between the launcher and the net service |
| `crates/api` | the cloud's API, shared by the net service and the [cloud](https://github.com/anyplaynet/cloud) |
| `crates/voice` | `anyplay-voice`, the party voice engine |
| `crates/proof` | `anyplay-proof`, device keys, proofs of possession and PKCE for the cloud API |
| `crates/fake-game` | `anyplay-fake-game`, a game's side of the bridge, for tests and for trying the network without a game |
| `crates/share`, `crates/share-worker` | the media path for share screen and share play, and the worker program that captures, encodes and shows the picture, records clips and broadcasts |
| `crates/discord` | an optional Discord Social SDK seam, off by default ([ADR 0022](docs/adr/0022-discord-social-sdk.md)) |
| `crates/probe` | `anyplay-probe`, which measures a tunnel |
| `spec/` | the network's own specifications: [bridge protocol](spec/bridge-protocol.md), [control protocol](spec/control-protocol.md), [cloud API](spec/cloud-api.md), [portal API](spec/portal-api.md), [web API groups](spec/web-api/README.md) and more |
| `docs/` | [design](docs/DESIGN.md), [social layer](docs/SOCIAL.md), [decisions](docs/adr), [which games can play online](docs/games.md), [privacy and security](docs/privacy-and-security.md), [how games are observed](docs/OBSERVING.md), [known gaps](docs/gaps.md), [supply chain](docs/supply-chain.md) |

## What works today

The network without games is live at anyplaynet.com, and the launcher uses it.

| Area | State |
|---|---|
| Accounts, friends, presence, messages, notifications | Done and live. |
| Parties with voice and push to talk | Done and live. |
| Share screen to a party | Done for the streamer on Windows, Linux and macOS. Game sound comes from Windows and macOS. A Mac cannot watch yet. |
| Broadcast to Twitch, YouTube or RTMP | Done on Windows, Linux and macOS; `rtmp://` and `rtmps://`. The game's sound is sent from Windows; elsewhere the track is silence. |
| Clips (the last seconds of a game, saved on the PC) | Done on Windows, Linux (GPU encode through NVENC or VA-API) and macOS. |
| Join links, Find Players, played with recently, captures and clips sent to friends | Done. |
| Bridge: sign-in, identity, NAT type, web API forwarding, push events, online-permission check, peer-to-peer datagrams and streams | Merged in the fork, speaking bridge protocol 1: the bridge and the net service prove the game start's token to each other with an HMAC challenge, and protocol 0 is no longer spoken ([ADR 0039](docs/adr/0039-the-bridge-proves-the-token-instead-of-sending-it.md)). The end-to-end CI that runs the bridge against the net service is in review. |
| Web API groups answered for games | `userProfile` and `communicationRestrictionStatus`. Every other group is refused and logged. |
| A first game played together over the internet | Not yet. This is phase 4 of the [roadmap](docs/DESIGN.md#roadmap). |
| Measuring tunnels between two real networks | Waiting for a second network (phase 0). |
| Share play (taking over a friend's pad) | Built in the net service, the cloud and the worker, and tested with fakes. The engine side (frame ring, guest pad, a second user) is in review in the fork. The launcher keeps it hidden and the public cloud keeps it off until a legal review. |
| Several cloud nodes | Not built. The cloud runs as one node. |

[docs/gaps.md](docs/gaps.md) lists what each part still lacks.

## The line

The network goes as far as the law allows and no further ([ADR 0021](docs/adr/0021-the-line-is-drawn-at-the-law.md)). It never contacts Sony's servers, uses Sony's code, keys or leaked documents, issues tokens for a platform or a publisher, poses as a console, or bypasses anti-cheat or copy protection. Formats are learned by observing games the player owns and written down as our own specifications. Names carry no platform trademarks. A game is a test case for a generic feature, never a feature of its own.

## Build and contribute

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Every pull request runs these on Windows, Linux and macOS. The parsers are fuzzed every night ([fuzz/](fuzz/README.md)). Start with the [design](docs/DESIGN.md); the words are in [CONTEXT.md](CONTEXT.md). Issues and pull requests are welcome; see [CONTRIBUTING.md](CONTRIBUTING.md). The rules shared by every AnyPlay repository are in the [organisation's CONTRIBUTING.md](https://github.com/anyplaynet/.github/blob/main/CONTRIBUTING.md).

### macOS

`cargo build --release --locked -p anyplay-net --features devices` builds a native service with CoreAudio microphone and speaker support. CI builds and tests it on macOS as well as Windows and Linux. The launcher embeds the matching binary at build time with `ANYPLAY_NET_EMBED` and starts it over standard input/output with an explicit per-user data folder.

Run standalone with `--data <folder>` or use its default under `~/Library/Application Support/AnyPlay/net`. Sign-ins use the macOS Keychain when available; `--file-credentials` is intended for isolated tests. Microphone access belongs to the launcher app's privacy prompt. On a Mac the share worker captures with ScreenCaptureKit and encodes with VideoToolbox ([ADR 0036](docs/adr/0036-macos-captures-with-screencapturekit-and-encodes-with-videotoolbox.md)), for share screen, clips and broadcast, and needs the Screen Recording permission. Discord's optional SDK is a separate integration, and there is no game engine bridge for macOS.

## Community

Questions and ideas: the [AnyPlay Discord](https://discord.gg/9zYqf8fEsh). More about AnyPlay at [anyplaynet.com](https://anyplaynet.com).

## License

MIT or Apache-2.0, at your option, so that AnyPS5 (GPL-2.0) and the cloud (AGPL-3.0) can both use these crates.

AnyPlay is a community project, not affiliated with the AnyPS5 maintainers or any platform holder. Only games you own.
