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
  - `signed_in` is 1 while the player is signed in to the network and not banned, else 0. The online-permission check does not follow it for now ([ADR 0008](../docs/adr/0008-the-online-permission-check-waits.md)).
  - `account_id` and `handle` are 0 and empty while signed out. A handle has at most 16 ASCII characters.
  - `nat_type` is 1, 2 or 3 as the platform counts NAT types, or 0 while unknown. Because a relay is always reachable, the net service reports at most 2.
  - `address` is the player's own virtual address, 0 when it has none.
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
