# AnyPlay Network

Online play for games that run on [AnyPS5](https://github.com/boykopovar/AnyPS5): accounts, friends, presence, sessions, invitations, and encrypted connections between players over the internet. No open ports and no shared LAN needed.

**Status: in development.** The net service, the fake game and the tunnel probe run, and two players meet, become friends and exchange game packets end to end in the tests of [anyplay-gg/cloud](https://github.com/anyplay-gg/cloud). No real game plays online yet: the bridge in AnyPS5 is under review, and the formats of games are learned one by one ([docs/gaps.md](docs/gaps.md)). Start with the [design](docs/DESIGN.md). The words we use are in [CONTEXT.md](CONTEXT.md) and the decisions behind them in [docs/adr](docs/adr).

## How it fits together

```
 Player A's PC                                    Player B's PC
 ┌─────────────────────────┐                      ┌─────────────────────────┐
 │ game                    │                      │ game                    │
 │  └ AnyPS5 libraries     │                      │  └ AnyPS5 libraries     │
 │     └ bridge            │                      │     └ bridge            │
 │         │ loopback      │                      │         │ loopback      │
 │ launcher ─ net service ◄┼──── tunnel (QUIC) ───┼► net service ─ launcher │
 └────────────┬────────────┘   direct or relay    └────────────┬────────────┘
              │                                                │
              └──────────────────►  cloud  ◄───────────────────┘
                  accounts · friends · presence · sessions · matchmaking
                  invitations · signaling · relays
```

- The **bridge** lives in an AnyPS5 build. When a game calls one of its network libraries, the bridge forwards the call to the net service on the same PC.
- The **net service** holds the player's account, talks to the cloud and carries game traffic to other players, directly or through a relay.
- The **cloud** keeps accounts, friends, sessions and invitations, and runs the relays. It lives in [anyplay-gg/cloud](https://github.com/anyplay-gg/cloud).

## Which games can play online

Games whose online part uses only the platform's own services: sessions, matchmaking and peer-to-peer connections. Games that need a publisher's own servers stay offline, because the network never stands in for them. [CONTEXT.md](CONTEXT.md) defines the title classes.

## Layout

| Path | What it is |
|---|---|
| `crates/net` | `anyplay-net`, the net service: account, cloud connection, bridge server, tunnels and game traffic |
| `crates/bridge-protocol` | the messages between the bridge and the net service |
| `crates/control` | the messages between the launcher and the net service |
| `crates/api` | the cloud's API, shared by the net service and the cloud |
| `crates/fake-game` | `anyplay-fake-game`, a game's side of the bridge, for tests and for trying the network without a game |
| `crates/probe` | `anyplay-probe`, which measures a tunnel between two networks (phase 0) |
| `spec/` | the network's own specifications: [bridge protocol](spec/bridge-protocol.md), [control protocol](spec/control-protocol.md), [cloud API](spec/cloud-api.md), [own API group](spec/own-api-group.md), [observation log](spec/observation-log.md) |
| `docs/` | [design](docs/DESIGN.md), [decisions](docs/adr), [how games are observed](docs/OBSERVING.md) and [known gaps](docs/gaps.md) |

## Try it

Measure a tunnel between two networks, for example your home and a phone's hotspot:

```sh
cargo run --release -p anyplay-probe -- listen          # on one side; prints a ticket
cargo run --release -p anyplay-probe -- connect <ticket> # on the other
```

It reports whether the tunnel went direct or through a relay, the round trip, loss, jitter and throughput. Without `--relay` it uses the public relays of the iroh project, because the network's own relays do not run yet; only probe packets cross them.

The launcher starts the net service by itself. To run one by hand: `cargo run -p anyplay-net -- --data <folder>`, then type control protocol lines.

## License

MIT or Apache-2.0, at your option. The permissive license lets AnyPS5 (GPL-2.0) and the cloud (AGPL-3.0) both use these crates.
