# AnyPlay Network

Online play for games that run on [AnyPS5](https://github.com/boykopovar/AnyPS5): accounts, friends, parties, voice and encrypted connections between players over the internet. Players open no ports and need no shared LAN. The network never contacts a platform holder's servers.

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

Players use the network through the [AnyPlay Launcher](https://github.com/anyplaynet/launcher), which starts the net service itself. To measure a tunnel between two networks, for example your home and a phone's hotspot:

```sh
cargo run --release -p anyplay-probe -- listen          # on one side; prints a ticket
cargo run --release -p anyplay-probe -- connect <ticket> # on the other
```

It reports whether the tunnel went direct or through a relay, the round trip, loss, jitter and throughput. Add `--relay-only` on either side to measure the relayed path. `listen --key <file>` keeps the listener's identity in a file (created with mode 0600 when missing), so the ticket stays the same when the listener restarts; without it every start makes a new ticket. The probe uses the iroh project's public relays, because the network's own relays do not run yet; only probe packets cross them.

## What's inside

| Path | What it is |
|---|---|
| `crates/net` | `anyplay-net`, the net service on each player's PC: account, cloud connection, bridge server, tunnels, voice and screen sharing |
| `crates/bridge-protocol` | the messages between the bridge and the net service |
| `crates/control` | the messages between the launcher and the net service |
| `crates/api` | the cloud's API, shared by the net service and the [cloud](https://github.com/anyplaynet/cloud) |
| `crates/voice` | `anyplay-voice`, the party voice engine |
| `crates/fake-game` | `anyplay-fake-game`, a game's side of the bridge, for tests and for trying the network without a game |
| `crates/probe` | `anyplay-probe`, which measures a tunnel |
| `spec/` | the network's own specifications: [bridge protocol](spec/bridge-protocol.md), [control protocol](spec/control-protocol.md), [cloud API](spec/cloud-api.md), [portal API](spec/portal-api.md), [web API groups](spec/web-api/README.md) and more |
| `docs/` | [design](docs/DESIGN.md), [social layer](docs/SOCIAL.md), [decisions](docs/adr), [which games can play online](docs/games.md), [privacy and security](docs/privacy-and-security.md), [how games are observed](docs/OBSERVING.md), [known gaps](docs/gaps.md) |

## Status

In development. In the end-to-end tests of [anyplaynet/cloud](https://github.com/anyplaynet/cloud), two players sign in, become friends, open a session and exchange game packets through a tunnel, directly and through a relay. Parties, voice, calls, join codes and screen sharing are built on the same tunnels. No real game plays online yet: the bridge in AnyPS5 is under review, and game formats are learned one by one ([docs/gaps.md](docs/gaps.md)).

## Build and contribute

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Every pull request runs these on Windows and Linux. The parsers are fuzzed every night ([fuzz/](fuzz/README.md)). Start with the [design](docs/DESIGN.md); the words are in [CONTEXT.md](CONTEXT.md). Until the network is live, only the AnyPlay team opens issues and pull requests; the shared rules are in [CONTRIBUTING.md](https://github.com/anyplaynet/.github/blob/main/CONTRIBUTING.md).

## Community

Questions and ideas: the [AnyPlay Discord](https://discord.gg/9zYqf8fEsh). More about AnyPlay at [anyplaynet.com](https://anyplaynet.com).

## License

MIT or Apache-2.0, at your option, so that AnyPS5 (GPL-2.0) and the cloud (AGPL-3.0) can both use these crates.

AnyPlay is a community project, not affiliated with the AnyPS5 maintainers or any platform holder. Only games you own.
