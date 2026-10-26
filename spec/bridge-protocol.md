# Bridge protocol

The messages between the bridge in an AnyPS5 build and the net service. Version 0, a draft: it changes without notice until version 1.

The Rust reference is [`crates/bridge-protocol`](../crates/bridge-protocol). The bridge side lives in C++ inside AnyPS5 and must match this file, not the Rust code.

## Connection

- The launcher starts the game with two variables:
  - `ANYPS5_NET_ENDPOINT`: `127.0.0.1:<port>` where the net service listens;
  - `ANYPS5_NET_TOKEN`: a random secret for this one game start, as lowercase hex.
- Without both variables the bridge stays off, and every network library behaves as it does today: offline and signed out.
- The bridge opens one TCP connection to the endpoint and sends **hello** as its first frame. The net service answers **welcome** or **reject**, then closes the connection after a reject. Nothing else is sent before welcome.
- The net service accepts connections from loopback addresses only, and one connection per token at a time; a new connection with the same token replaces the old one.
- When the connection breaks, the bridge reports the player as signed out to the game and connects again every 2 seconds with the same token.

## Frames

Every message is one frame. All integers are little-endian.

| Offset | Size | Field | Meaning |
|---|---|---|---|
| 0 | 4 | `length` | Payload length in bytes, at most 16 MiB (16,777,216). |
| 4 | 2 | `kind` | What the payload is; see below. |
| 6 | 2 | `flags` | Senders write 0. Receivers ignore it in version 0. |
| 8 | 4 | `request` | 0 for an event. A request carries a number unique on this connection, and its reply carries the same number. |
| 12 | `length` | payload | Depends on `kind`. |

A receiver that reads a `length` above the limit closes the connection. A frame with an unknown `kind` is skipped, so either side can be newer than the other.

- An **event** has `request` 0 and gets no answer.
- A **request** has a `request` number other than 0. Its **reply** has the same number and the kind of the request with the top bit set (`kind | 0x8000`).
- A receiver that does not know the kind of a request answers with **unsupported** (`0x8000`, empty payload) and the request's number.
- Only the bridge sends requests in version 0. Several may be open at once; replies can come in any order.

## Payloads

Payloads are fields one after the other, without padding:

| Type | Encoding |
|---|---|
| `u8`, `u16`, `u32`, `u64`, `i32` | little-endian integers |
| `addr` | an IPv4 address as its 32-bit number, little-endian like every integer (`198.18.0.1` is `0xC6120001`, sent as `01 00 12 C6`) |
| `str` | a `u16` byte count, then that many bytes of UTF-8, without a terminating zero |
| `bytes` | a `u32` byte count, then that many bytes |
| `pairs` | a `u16` count, then that many pairs of `str` (a name, then a value) |
| `list<str>` | a `u16` count, then that many `str` |

A payload may be longer than the fields its receiver knows; the receiver ignores the rest. New fields are only ever added at the end. A payload shorter than its fields is an error: the receiver skips an event like that and answers a request like that with unsupported.

## Kinds

### Handshake

| Kind | Name | Direction | Payload |
|---|---|---|---|
| `0x0001` | hello | bridge → service | `version: u16`, then the token as raw bytes (the decoded hex) |
| `0x0002` | welcome | service → bridge | `version: u16` (the version both sides will use), `udp_port: u16`, `tcp_port: u16`, `stream_key: 16 bytes` |
| `0x0003` | reject | service → bridge | a UTF-8 reason for the log |
| `0x8000` | unsupported | either way | empty; the reply to a request of unknown kind |

`udp_port` and `tcp_port` are the net service's loopback ports for game traffic (see below); 0 means the net service carries no game traffic.

### Sign-in state (`0x01xx`)

| Kind | Name | Direction | Payload |
|---|---|---|---|
| `0x0101` | state | service → bridge | `signed_in: u8`, `account_id: u64`, `handle: str`, `nat_type: u8`, `address: addr` |
| `0x0102` | feature | bridge → service | `user_id: i32`, `feature: u32` |

- **state** comes right after welcome and again whenever a field changes. The bridge keeps the last one and answers the game's questions about sign-in, identity and NAT from it without waiting.
  - `signed_in` is 1 while the player is signed in to the network and not banned, else 0. The online-permission check does not follow it yet. [ADR 0021](../docs/adr/0021-the-line-is-drawn-at-the-law.md) makes it follow: signed in and not banned, for the AnyPlay network only.
  - `account_id` and `handle` are 0 and empty while signed out. A handle has at most 16 ASCII characters.
  - `nat_type` is 1, 2 or 3 as the platform counts NAT types, or 0 while unknown. The net service measures it with iroh's net report: 1 when the relays answer its UDP address probes and the public address they see is one of the PC's own; 2 when a relay answers (every peer can reach it through the relay) or the NAT shows the same public address to every relay; 3 otherwise. A net service on relay only is never 1. A change comes as a new `state`.
  - `address` is the player's own virtual address, 0 when it has none. It is the address the player has in the session the launcher shows, else the one the cloud names for the account. It can change when the player enters a session, and `state` is sent again then.
- **feature** reports that the game used an online feature it must announce, such as through the platform's feature notification. The net service only writes it to its observation log.

### Web API (`0x02xx`)

| Kind | Name | Direction | Payload |
|---|---|---|---|
| `0x0201` | web request | bridge → service, request | `user_id: i32`, `api_group: str`, `path: str`, `method: str`, `headers: pairs`, `body: bytes` |
| `0x8201` | web reply | service → bridge | `status: u16`, `headers: pairs`, `body: bytes` |
| `0x0202` | push filter | bridge → service | `user_id: i32`, `filter: u32`, `data_types: list<str>` |
| `0x0203` | push filter gone | bridge → service | `user_id: i32`, `filter: u32` |
| `0x0204` | push event | service → bridge | `user_id: i32`, `filter: u32`, `data_type: str`, `from: u64`, `to: u64`, `data: bytes`, `extended: pairs` |

- A **web request** carries a request exactly as the game built it: the user of its user context, the API group, the path with its query, the method, the headers the game added and the body.
- The **web reply**'s `status` is the HTTP status the game gets. Status 0 means the network does not offer this API; the bridge then answers the game as it does offline. The bridge waits at most 30 seconds for a web reply and treats a timeout as status 0.
- A **push filter** tells the net service which push events the game wants, by data type; `filter` is the bridge's number for the filter. A **push event** names the filter it matches. `from` and `to` are account IDs, 0 when not known.

### Session signaling and game traffic (`0x03xx`)

| Kind | Name | Direction | Payload |
|---|---|---|---|
| `0x0301` | activate | bridge → service, request | `user_id: i32`, `session: str`, `session_kind: u8` |
| `0x8301` | activated | service → bridge | `result: u8` |
| `0x0302` | deactivate | bridge → service | `user_id: i32`, `session: str` |
| `0x0303` | peer | service → bridge | `session: str`, `account_id: u64`, `handle: str`, `address: addr`, `peer_state: u8` |
| `0x0304` | peer info | bridge → service, request | `address: addr` |
| `0x8304` | peer info reply | service → bridge | `known: u8`, `rtt_us: u32`, `relayed: u8` |
| `0x0305` | bind | bridge → service, request | `vport: u16`, `transport: u8`, `loopback_port: u16` |
| `0x8305` | bound | service → bridge | `result: u8` |
| `0x0306` | unbind | bridge → service | `vport: u16`, `transport: u8` |

- **activate** starts tunnels to the other members of a session the game uses; `session_kind` is 1 for a player session and 2 for a game session. `result` is 0 (done), 1 (the network knows no such session) or 2 (signed out).
- **peer** reports each other member of an activated session: once when it is known and again whenever `peer_state` changes. `peer_state` is 1 connecting, 2 connected, 3 disconnected or 4 failed.
- **peer info** asks for the measured round trip of a peer; `known` is 0 for an address the net service does not know, and `relayed` is 1 while the tunnel runs through a relay.
- **bind** tells the net service where game traffic for a virtual port goes: `transport` is 1 for datagrams and 2 for streams, and `loopback_port` is the bridge's loopback socket for it (a UDP socket for datagrams, a listening TCP socket for streams). `result` is 0 (bound) or 1 (the virtual port is already bound for this transport).

#### Game traffic

Game packets do not travel in frames.

- **Datagrams.** The bridge sends each datagram from the loopback UDP socket it bound, to `127.0.0.1:<udp_port>`. Each starts with an 8-byte header: `address: addr` (the peer), `vport: u16` (the peer's port), `source_vport: u16` (the game socket's own port), then the game's bytes. The net service drops datagrams from loopback ports nobody bound. It delivers a datagram from a peer to the bound loopback port, with the same header where `address` is the sender's virtual address, `vport` the receiving port and `source_vport` the sender's port.
- **Streams.** For a reliable peer-to-peer stream the bridge opens a TCP connection to `127.0.0.1:<tcp_port>` and sends `stream_key` (the 16 bytes from welcome) and then the 8-byte header. After that the connection carries the game's bytes both ways, and closing it closes the stream. For a stream from a peer, the net service connects to the bound listening port and sends the same 16-byte key and an 8-byte header naming the sender.

#### Broadcasts and the virtual LAN

A game that looks for others on a local network sends a datagram to a broadcast address. The net service carries such a datagram to every other member of the player's session, so that the session behaves like one LAN. This is the virtual LAN of [ADR 0021](../docs/adr/0021-the-line-is-drawn-at-the-law.md): the network answers a game's local-network calls between the members of one session and nothing else.

**No new frame.** A broadcast is an ordinary datagram. The bridge sends it from the loopback UDP socket it bound, to `127.0.0.1:<udp_port>`, with the usual 8-byte header, and `address` is one of two broadcast destinations:

| Destination | Address |
|---|---|
| limited broadcast | `255.255.255.255` |
| broadcast of the virtual subnet | `198.19.255.255` (the subnet is `198.18.0.0/15`, netmask `255.254.0.0`) |

`vport` is the port the game sent to, `source_vport` the port of the sending socket. The tunnels do not change, and a net service that does not know broadcasts counts the datagram as sent to an unknown peer and drops it. No other destination is a broadcast: multicast (`224.0.0.0/4`, mDNS) and the broadcast address of any real network are not carried, and the bridge drops them.

**Who receives it.** The net service copies the datagram into the tunnel of every other member of the player's current session whose tunnel is up at that moment: the session the launcher shows, the one that gave the player its address in `state` ([ADR 0020](../docs/adr/0020-virtual-addresses-belong-to-sessions.md)). For a LAN game that is the `lan` session the launcher opened when the player pressed Play Together ([ADR 0030](../docs/adr/0030-play-together-is-a-session-that-ends-with-its-host.md), [Play Together](play-together.md)), whether the player hosts it or joined it: it is the game's only session, so it is the newest. The audience follows the session as members come and go and is empty while the player is in no session. The sender gets no copy, and a member whose tunnel is not up gets none later, as on a LAN. A broadcast needs no `activate`: a game that never calls the session functions (LAN modes usually do not) reaches its session all the same.

**What a receiver sees.** Each copy arrives like any datagram from a peer: `address` is the sender's virtual address as the receiving net service knows it, `vport` the receiving port, `source_vport` the sender's port. It is delivered only when the receiver has bound `vport` for datagrams; else it is dropped and counted as an unbound port. Nothing marks it as a broadcast: a receiver cannot tell it from a unicast datagram and has no need to. A reply to the sender is an ordinary datagram to `address` and `source_vport`.

**Limits.** They are the sending net service's, per player, and excess datagrams are dropped silently (UDP does not promise delivery):

- at most 1,200 bytes of game data per datagram;
- at most 40 broadcasts per second, with a burst of 80;
- at most 32 KiB of game data per second, counting 8 bytes per datagram for its header;
- any `vport` the receiver bound, with no allow-list. The receiver's own `bind` decides who listens, and the limits above bound the cost whichever port a game chooses.

Unicast datagrams are not limited by these.

**Nothing leaves the session.** The net service sends a broadcast only into tunnels, and a tunnel exists only between members of a session they share. It never writes to a real network interface, never to a member of another session (a member of two sessions gets the broadcasts of the sender's current session only), and never answers a broadcast itself. The bridge must hold up its side: it never sends a broadcast or multicast datagram of the game to the real network.

**Counters.** The net service counts broadcasts accepted, copies made, broadcasts with nobody to send to, and those dropped for size or rate, and writes the totals to its log (target `anyplay::broadcast`) once a minute while they change. The counts name no one.

##### What the bridge does with the game's sockets

This is the whole of the bridge's part:

1. **Bind every UDP socket.** Each UDP socket the game binds, including one bound to port 0 (the bridge picks the number, and the game sees it as the local port), is sent to the net service with **bind**, with `vport` equal to the local port the game sees. When two sockets share a port (`SO_REUSEADDR`, `SO_REUSEPORT`), the bridge binds it once and delivers each incoming datagram to every socket on that port. A socket is bound before it first sends, so its datagrams are not dropped as coming from an unbound socket.
2. **Present the virtual interface.** While `state.address` is not 0, the game's interface queries show an interface with that address, netmask `255.254.0.0`, broadcast address `198.19.255.255` and MTU 1,200. It is the only non-loopback interface with an IPv4 address; IPv6 stays unavailable. Without it a game computes no subnet broadcast and gathers no candidates the peers can reach. While `state.address` is 0, the bridge presents what it presents today.
3. **Send.** A `sendto` to either broadcast address from a UDP socket with `SO_BROADCAST` set becomes one datagram to the net service with that `address`, `vport` the destination port and `source_vport` the socket's local port. Without `SO_BROADCAST` the call fails as it does on the platform. The bridge reports the send as done at once.
4. **Receive.** A datagram the net service delivers to a bound loopback port is handed to the game's socket as received from `address:source_vport`, with the game's bytes unchanged. A broadcast arrives like a unicast datagram, so a socket bound to `0.0.0.0` or to the virtual address receives it. The bridge does not loop the game's own broadcasts back to it.
5. **Do not touch the real network** for a datagram to a virtual address or a broadcast address. A datagram to any other address fails at once with "network unreachable", so a game's attempt to reach a server on the internet (a STUN server, say) ends in an error and not in a timeout.

##### Connections between players inside the virtual LAN

A game may find another by broadcast and then open a peer-to-peer connection over UDP with ICE, DTLS and SCTP (WebRTC). Inside the virtual LAN that works with the datagram path above and needs nothing else:

- **Candidates are the virtual addresses.** Because the game sees one interface with the virtual address (step 2), the host candidates it gathers are that address with the port of the ICE socket, and the candidates it receives from a peer are the peer's virtual address with the peer's port. The ICE socket is bound with `vport` equal to its local port (step 1), so a connectivity check addressed to a candidate reaches the peer's socket. The address in a peer's candidates is the peer's own address in its session, which is the address this PC holds for it except after the rare clash that [ADR 0020](../docs/adr/0020-virtual-addresses-belong-to-sessions.md) describes.
- **No STUN, no TURN.** A host candidate on the virtual subnet is reachable by every member, so a connection needs no reflexive or relayed candidate. The net service runs no STUN or TURN server, issues no credentials for one, reads no game payload, and rewrites no address. A relayed tunnel (see `peer info`) only adds delay that ICE's timeouts already allow for.
- **Unicast on any port.** Every datagram between two members goes by `address` and `vport` on any bound port, up to 64 KiB. A datagram that fits goes as one QUIC datagram (about 1,200 bytes) and a larger one over a stream of its own, which is why the interface reports an MTU of 1,200: DTLS and SCTP then fit in single datagrams. Order and delivery are not promised.
- **Signals after discovery.** Some games, NetherNet among them, send the offer, the answer and the candidates as unicast datagrams to the address and port the other side's last datagram came from. That is why `source_vport` must be the sending socket's local port and `address` the sender's virtual address.

Services of the platform or of a publisher that a game uses next to this (a session directory, a signaling server, a STUN or TURN server of a publisher) are not reached and not answered. The network does not stand in for them ([ADR 0021](../docs/adr/0021-the-line-is-drawn-at-the-law.md)).

##### What the games do (Minecraft: Bedrock Edition)

The first user is Minecraft: Bedrock Edition in its LAN mode. What public sources say about it, as of 25 October 2026:

| | RakNet (up to 26.50) | NetherNet (from 26.60) |
|---|---|---|
| Discovery | An unconnected ping (`0x01`) to the broadcast address on UDP 19132 (19133 for IPv6); the host answers with an unconnected pong that carries the server name. | An encrypted request on UDP 7551 to `255.255.255.255`, sent every 2 seconds by a client that is looking; the host answers with a response that carries name, level, game mode and player counts. Packets are AES-ECB under the SHA-256 of `0xdeadbeef` with an HMAC-SHA-256 in front. The net service does not read them. |
| Answer | To the sender of the ping. | An implementation answers the request's sender by unicast; the specification of the format says "broadcast back". The network handles both. |
| Connection | RakNet over UDP with the pong's source. | After discovery both sides exchange signals (`CONNECTREQUEST` with the SDP offer, `CONNECTRESPONSE`, `CANDIDATEADD`) as discovery message packets on UDP 7551, by unicast to the address and port the other side's last packet came from. Then WebRTC: ICE over UDP, DTLS, SCTP, with a reliable and an unreliable data channel. |
| Candidates | none | Host candidates of the local interfaces. Mojang's configuration for the HTTP path has no STUN or TURN servers, UDP candidates only and no trickle ICE; the LAN implementations pass no ICE servers either. |

- RakNet is deprecated in 26.60 and NetherNet is the default transport of dedicated servers since 26.50 ([Bedrock Edition protocol](https://minecraft.wiki/w/Bedrock_Edition_protocol)); 26.60 is due on 27 October 2026.
- Sources: [NetherNet, Minecraft Wiki](https://minecraft.wiki/w/NetherNet); [NetherNet HTTP Signaling, Partner Onboarding Guide, Mojang](https://mojang.github.io/bedrock-protocol-docs/guides/nether-net-onboarding-guide) (configuration: no STUN or TURN, UDP only, full ICE); [df-mc/nethernet-spec](https://github.com/df-mc/nethernet-spec) (LAN discovery, packet types, signal format, candidate format); [df-mc/go-nethernet, discovery/listener.go](https://github.com/df-mc/go-nethernet/blob/main/discovery/listener.go) (default broadcast `255.255.255.255:7551`, request every 2 seconds, unicast answer, signals to the last address seen for a network ID); [RakNet, Minecraft Wiki](https://minecraft.wiki/w/RakNet) (unconnected ping and pong); [bedrock-v nethernet](https://vpm.vlang.io/packages/bedrock-v.nethernet) (a LAN channel hands out no ICE servers; a server binds 7551).
- No public source describes the console builds' exact behaviour. The observation log of the first run settles three open points for the bridge: whether the game answers a request by broadcast or by unicast, which interface call it gathers its addresses with, and whether it sends to the limited broadcast, the subnet broadcast or both. The spec above works for each answer.

##### Considered options

- **A frame kind or a flag for broadcasts.** The header's `address` already says where a datagram goes, and a receiver needs no more than for a unicast. A new kind would make every old net service drop the datagram, which a datagram to an unknown address does anyway.
- **An allow-list of ports** (19132, 19133, 7551). Another game's discovery would need a new release of every net service. The receiver decides what it hears by binding, and the limits bound the cost, so the list would only add a place to be wrong.
- **Broadcast to every session of the game.** A member of two sessions would receive both, which is two LANs seen as one. The session the launcher shows already gives the player its address, so it is the one LAN the player stands in.
- **Receiver-side limits.** A receiver cannot tell a broadcast from a unicast, and a peer may send unicast at any rate to a bound port anyway, so a limit there would only slow honest game traffic.

### Join intents and the invitation dialog (`0x04xx`)

| Kind | Name | Direction | Payload |
|---|---|---|---|
| `0x0401` | intent | service → bridge | `user_id: i32`, `intent_type: str`, `properties: pairs` |
| `0x0402` | invitation dialog | bridge → service, request | `user_id: i32`, `session: str`, `max: u16` |
| `0x8402` | invitation dialog reply | service → bridge | `result: u8`, `sent: u16` |

- An **intent** is held for the game to pick up, such as a join intent (`intent_type` `joinSession`) with the session in its properties. The net service sends a pending intent right after welcome. `user_id` -1 means the initial user.
- The **invitation dialog** request is answered when the player has picked friends in the launcher or closed the picker: `result` 0 means invitations went out (`sent` of them), 1 that the player cancelled. The bridge waits for as long as the game keeps its dialog open.

### Share play (`0x05xx`)

Drafted in the [share play spec](share-play.md#bridge-protocol-additions): opening and closing the frame ring, the share mode, the guest's pad states, and the host's override. Not implemented.

## Observation

A bridge writes an [observation log](observation-log.md) when `ANYPS5_NET_OBSERVE` names a file, with or without a net service.
