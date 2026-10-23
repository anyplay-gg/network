# Bridge protocol

The messages between the bridge in an AnyPS5 build and the net service. Version 0, a draft: it changes without notice until version 1.

The Rust reference is [`crates/bridge-protocol`](../crates/bridge-protocol). The bridge side lives in C++ inside AnyPS5 and must match this file, not the Rust code.

## Connection

- The launcher starts the game with two variables:
  - `ANYPS5_NET_ENDPOINT`: `127.0.0.1:<port>` where the net service listens;
  - `ANYPS5_NET_TOKEN`: a random secret for this one game start, as lowercase hex.
- Without both variables the bridge stays off, and every network library behaves as it does today: offline and signed out.
- The bridge opens one TCP connection to the endpoint and sends **hello** as its first frame. The net service answers **welcome** or **reject**, then closes the connection after a reject. Nothing else is sent before welcome.
- The net service accepts connections from loopback addresses only.

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

## Kinds

| Kind | Name | Direction | Payload |
|---|---|---|---|
| `0x0001` | hello | bridge → service | `version: u16`, then the token as raw bytes (the decoded hex) |
| `0x0002` | welcome | service → bridge | `version: u16`, the protocol version both sides will use |
| `0x0003` | reject | service → bridge | a UTF-8 reason for the log |

Planned groups, specified once their phase starts (see the [roadmap](../docs/DESIGN.md#roadmap)):

| Range | Group | Phase |
|---|---|---|
| `0x01xx` | sign-in state, identity, online permission, NAT information | 2 |
| `0x02xx` | web API requests, responses and push events | 2 |
| `0x03xx` | session signaling | 3 |
| `0x04xx` | join intents and the invitation dialog | 4 |

## Game traffic

Game packets do not travel in frames. They use a separate loopback UDP socket with its own small header, specified together with the session signaling group in phase 3.
