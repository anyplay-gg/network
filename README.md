# AnyPlay Network

Online play for games that run on [AnyPS5](https://github.com/boykopovar/AnyPS5): accounts, friends, presence, sessions, invitations, and encrypted connections between players over the internet. Players open no ports and need no shared LAN. The network never talks to Sony's servers.

**Status: in development.** The net service, the fake game and the tunnel probe run. In the end-to-end tests of [anyplaynet/cloud](https://github.com/anyplaynet/cloud), two players sign in, become friends, open a session and exchange game packets through a tunnel, directly and through a relay. No real game plays online yet: the bridge in AnyPS5 is under review, and the formats of games are learned one by one ([docs/gaps.md](docs/gaps.md)). Start with the [design](docs/DESIGN.md); the words are in [CONTEXT.md](CONTEXT.md) and the decisions in [docs/adr](docs/adr). Questions and ideas: the [AnyPlay Discord](https://discord.gg/nSHbHBRdS).

## The idea

AnyPS5 already replaces the console's system libraries with its own code. So the line between a game and the network is a function call inside AnyPS5, not encrypted traffic on a wire. The network plugs in at that call. It needs no DNS redirection, no TLS interception, no patched game code and no contact with Sony. [RPCN](https://github.com/RipleyTom/rpcn) has carried PS3 games the same way since 2019; nobody had built it for the session and signaling services PS5 games use.

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

## The three parts

**The bridge** lives in an AnyPS5 build and is small on purpose: it forwards and does not translate. The launcher switches it on with a loopback address and a secret for one game start. It then holds one TCP connection to the net service and speaks the [bridge protocol](spec/bridge-protocol.md): length-prefixed binary frames with request numbers, where web API bodies pass through as opaque bytes. The engine needs no JSON, WebSocket or ICE code. Without the variables every library behaves as it does today, offline and signed out.

**The net service** (`anyplay-net`, this repository) is the program on each player's PC. The launcher starts it and talks to it in JSON lines over its standard streams ([control protocol](spec/control-protocol.md)). One task owns its state and handles commands, cloud events, bridge frames and tunnel changes one at a time. Game packets never pass that task: they go from the bridge's loopback socket straight into the tunnel.

- It keeps the account's refresh token in the operating system's credential store.
- It holds one WebSocket to the cloud for presence, invitations and session changes, and reconnects with backoff and jitter.
- It turns the platform-shaped requests a game sends into the cloud's own API, one handler per API group. What it does not know, it refuses and writes to the [observation log](spec/observation-log.md).
- It opens [iroh](https://www.iroh.computer/) tunnels to other players: QUIC with hole punching, end-to-end encryption keyed to each device, and relays as the fallback ([ADR 0002](docs/adr/0002-tunnels-on-iroh.md)). Each player in a session gets a virtual address from `198.18.0.0/15`, a range reserved for testing and never routed on the internet. The game sends to that address and the tunnel carries it.
- It checks game version and AnyPS5 build before every join, so only matching versions play together.

**The cloud** keeps accounts, friends, presence, sessions and invitations, and signs the tickets that let a player use a relay. It lives in [anyplaynet/cloud](https://github.com/anyplaynet/cloud): one program that runs on a home server with `docker compose up` and is being built to run as a cluster. The launcher can point at any cloud, so nobody depends on one operator.

## A join, step by step

1. A is playing. The game creates a session; bridge, net service and cloud record it. A's friends see "playing X · joinable".
2. B selects **Join** in the launcher or accepts an invitation. The version guard checks both sides, the launcher starts the game, and the net service holds a join intent.
3. The game picks up the intent and joins the session. The cloud adds B and tells A.
4. Both games activate session signaling. The net services exchange tunnel keys through the cloud and connect, directly or through a relay. Each game gets the other player's virtual address.
5. Game traffic flows through the tunnel. The cloud never sees it.

## Which games can play online

The network answers the platform's own services for online play with AnyPlay accounts. It can also bring back a game's online play after its publisher has shut the service down, with a server written from observation. It never stands in for a publisher service that still runs, and never issues tokens for anyone else's client. A game whose online play runs on such a live service stays offline on the PlayStation side ([the line is drawn at the law](docs/adr/0021-the-line-is-drawn-at-the-law.md)).

| Class | The game's online part | On the network |
|---|---|---|
| A | Platform services only: sessions, matchmaking, peer-to-peer play, one player hosting | the goal |
| B | Platform services plus a publisher backend | only if the game also plays without the backend |
| C | A publisher backend only: own accounts, dedicated servers, live-service games | stays offline while the publisher runs it; a revival server only after the publisher has shut it down |
| 0 | No online part | nothing to do |

## Privacy and security

- **Encrypted end to end.** Tunnels are QUIC with keys bound to each device. A relay forwards bytes it cannot read.
- **Hidden addresses.** A player can keep every tunnel on the relay, so other players never learn their IP address (`--relay-only`, *Hide Your IP Address* in the launcher). The cloud publishes no player's direct addresses.
- **Relays admit only players.** A relay lets an endpoint in only with a ticket the cloud signed for that endpoint. It checks the ticket on its own and never calls the cloud ([ADR 0007](docs/adr/0007-relays-check-signed-tickets.md)).
- **Loopback only.** The net service accepts bridge connections from loopback with a secret for one game start, and the launcher's channel is the process's own standard streams, so no other program can speak for the player.
- **Nothing from Sony.** No servers, code, keys, certificates or leaked documents. Formats come from what games send through the bridge, written down in [spec/](spec) separately from the code.

## Repository

| Path | What it is |
|---|---|
| `crates/net` | `anyplay-net`, the net service: account, cloud connection, bridge server, tunnels and game traffic |
| `crates/bridge-protocol` | the messages between the bridge and the net service |
| `crates/control` | the messages between the launcher and the net service |
| `crates/api` | the cloud's API, shared by the net service and the cloud |
| `crates/fake-game` | `anyplay-fake-game`, a game's side of the bridge, for tests and for trying the network without a game |
| `crates/voice` | `anyplay-voice`, the party voice engine: microphone to Opus frames and frames to speakers, without a transport ([spec](spec/voice.md)) |
| `crates/probe` | `anyplay-probe`, which measures a tunnel between two networks |
| `fuzz/` | [fuzz targets](fuzz/README.md) for the parsers of both protocols, run every night |
| `spec/` | the network's own specifications: [bridge protocol](spec/bridge-protocol.md), [control protocol](spec/control-protocol.md), [cloud API](spec/cloud-api.md), [portal API](spec/portal-api.md), [own API group](spec/own-api-group.md), [observation log](spec/observation-log.md), [web API groups](spec/web-api/README.md) |
| `docs/` | [design](docs/DESIGN.md), [social layer](docs/SOCIAL.md), [decisions](docs/adr), [how games are observed](docs/OBSERVING.md), [known gaps](docs/gaps.md) and [supply chain](docs/supply-chain.md) |

## Try it

Measure a tunnel between two networks, for example your home and a phone's hotspot:

```sh
cargo run --release -p anyplay-probe -- listen          # on one side; prints a ticket
cargo run --release -p anyplay-probe -- connect <ticket> # on the other
```

It reports whether the tunnel went direct or through a relay, the round trip, loss, jitter and throughput. Add `--relay-only` on either side to measure the relayed path. Without `--relay` it uses the public relays of the iroh project, because the network's own relays do not run yet; only probe packets cross them. While a relay is reached, the ticket names no IP address, so it can be posted where others read it.

The launcher starts the net service by itself. To run one by hand: `cargo run -p anyplay-net -- --data <folder>`, then type control protocol lines.

## Testing

Every pull request runs formatting, Clippy with warnings as errors and all tests on Windows and Linux. The parsers of the bridge frames, the game traffic headers, the control lines and the voice frames are fuzzed for 15 minutes each every night, and for a minute on pull requests that touch them. The end-to-end tests in [anyplaynet/cloud](https://github.com/anyplaynet/cloud) run a cloud, a relay that checks tickets, two net services and two fake games in one process.

## License

MIT or Apache-2.0, at your option. The permissive license lets AnyPS5 (GPL-2.0) and the cloud (AGPL-3.0) both use these crates.
