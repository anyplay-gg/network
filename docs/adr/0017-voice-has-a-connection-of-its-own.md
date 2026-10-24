# Voice has a connection of its own

Between two party members the net service holds a second QUIC connection for voice, on the same iroh endpoint as their game tunnel, with the ALPN `anyplay/voice/0`. Game datagrams start with a virtual address and carry no tag byte, so voice could not share the tunnel's datagrams without changing the game traffic. The connection is its own, the path is not: both use one endpoint, so a direct or relayed path, hole punching, tickets and the relay-only setting apply to voice as they do to games, with no code for voice ([ADR 0010](0010-voice-and-shared-play-go-through-the-tunnels.md)).

Who may hold a voice connection is the party's member list. The cloud's party names each member's tunnel key, relay and whether they take part; a net service closes a connection whose handshake-proven key is not a current member's, and dials only members it finds there ([voice spec](../../spec/voice.md#connections)).

## Considered options

- **A tag byte in the tunnel's datagrams:** one connection per pair, but it changes the game traffic format that bridges already speak, costs a byte on every game packet, and gives voice no way to be closed or refused apart from the game.
- **A second iroh endpoint for voice:** a separate key, relay ticket and set of hole-punched paths per player, so relay-only and the cloud's tickets would need a line of their own. Rejected for the same reasons as a second stack in ADR 0010.
- **A uni-directional stream per frame:** reliable and ordered, which is what voice must not be: a late frame is worse than a lost one.
