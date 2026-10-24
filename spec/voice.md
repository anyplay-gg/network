# Voice frame

The datagram that carries 20 ms of a member's voice to the other members of a party. Version 0, a draft: it changes without notice until version 1.

The Rust reference is [`crates/voice`](../crates/voice) (`wire.rs`). Voice travels through the tunnels like game traffic ([ADR 0010](../docs/adr/0010-voice-and-shared-play-go-through-the-tunnels.md)); the choices behind the engine are in [ADR 0014](../docs/adr/0014-voice-is-an-engine-without-a-transport.md).

## Transport

One frame is exactly one datagram. A sender sends every frame to each other member of the party, on that member's tunnel, and never waits for an answer: a frame that is lost is not sent again.

A frame is at most 1200 bytes, the smallest datagram QUIC promises to carry whole, so no frame is ever split.

Frames are not mixed with game datagrams, whose first bytes are a virtual address. The tunnel to a member carries them on a QUIC connection of its own, with the ALPN `anyplay/voice/0`, between the same two endpoints. It then takes the same direct or relayed path as the game connection, and the relay-only setting applies to it without a line of its own.

## Frame

All numbers are big-endian.

| Offset | Size | Field | |
|---|---|---|---|
| 0 | 1 | version | 0 |
| 1 | 1 | flags | bit 0: end of talk; bits 1 to 7 must be 0 |
| 2 | 2 | sequence | counts the frames the sender sent, wrapping at 65536 |
| 4 | 4 | timestamp | the sender's sample clock at 48 kHz, wrapping at 2^32 |
| 8 | 1 to 1192 | payload | one Opus packet |

### Payload

One Opus packet ([RFC 6716](https://www.rfc-editor.org/rfc/rfc6716)) of exactly 20 ms, mono, coded for 48 kHz. Voice mode, in-band forward error correction on, discontinuous transmission on, a bitrate around 28 kbit/s with the variable bitrate constrained. The receiver may ask for none of this; the sender chooses, and the decoder reads whatever the packet says.

A packet of one or two bytes is the encoder's comfort noise: the sender is silent but present.

### Sequence and timestamp

The sequence number counts frames sent. The timestamp counts time: it advances by 960 for every 20 ms of the sender's microphone, whether a frame went out for it or not. Together they tell the receiver three things apart:

- the next sequence number with the next timestamp (previous plus 960): continuous speech;
- a sequence number missing, the timestamps still 960 apart per missing frame: loss. The receiver rebuilds the frame from the redundancy in the next one or lets the decoder conceal it;
- consecutive sequence numbers with a timestamp more than 960 further on: the sender sent nothing in between (a pause, or discontinuous transmission). The receiver starts a new talk, and waits for its own buffer depth again before it plays.

The first sequence number and timestamp of a sender are arbitrary. A receiver with nothing buffered and nothing playing takes the first frame it sees as the start, so a sender that restarts at any number is followed.

### End of talk

Flag bit 0 marks the last frame of a stretch of talking: the push-to-talk key was released, or voice activity ended. The receiver plays that frame and then goes quiet at once, with no concealment, and starts a new talk with the next frame. If the frame is lost, the receiver notices after eight missing frames (160 ms) instead; after a comfort-noise frame it waits 520 ms, which is longer than the 400 ms the sender takes between two of them.

The sender sends no frames between the end of one talk and the start of the next, apart from the comfort-noise frames of discontinuous transmission (one at the start, then one every 400 ms) while the key is held on silence.

## What a receiver must do

1. Refuse, without a reply, anything that is not a frame: shorter than 8 bytes, a version other than 0, a flag bit other than bit 0, an empty payload, or more than 1192 bytes of payload. The Rust reference returns the reason; the fuzz target `voice_frame` checks that every input is either read and written back to the same bytes, or refused for exactly one of these reasons.
2. Refuse a payload that does not hold exactly 960 samples at 48 kHz.
3. Keep at most 7 senders. A frame from an eighth is dropped.
4. Put frames back in order by sequence number (taking the wrap into account), drop duplicates and frames whose time has passed, and wait before a talk starts for a depth that follows the jitter measured on the frames (RFC 3550, 40 to 200 ms).

## Not in version 0

- No sender identity in the frame: the tunnel names the sender.
- No encryption of its own: the tunnel encrypts end to end.
- No stereo, no other frame sizes, no bitrate negotiation.
- No receiver reports. A sender cannot learn the loss it causes, so its `expected loss` is a setting.
