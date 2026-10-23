# AnyPlay Network

Online play for games that run on [AnyPS5](https://github.com/boykopovar/AnyPS5): accounts, friends, presence, sessions, invitations, and encrypted connections between players over the internet. No open ports and no shared LAN needed.

**Status: design.** Nothing runs yet. Start with the [design](docs/DESIGN.md). The words we use are in [CONTEXT.md](CONTEXT.md) and the decisions behind them in [docs/adr](docs/adr).

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

- `crates/bridge-protocol`: the messages between the bridge and the net service.
- `spec/`: the network's own specifications, written from observing games.
- `docs/`: design and decisions.

## License

MIT or Apache-2.0, at your option. The permissive license lets AnyPS5 (GPL-2.0) and the cloud (AGPL-3.0) both use these crates.
