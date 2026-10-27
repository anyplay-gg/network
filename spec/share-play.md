# Share play

How a player streams the game they are running to a friend in their party, who watches, takes over their pad, or plays beside them. Version 0, a draft: it changes without notice until version 1. Built so far: the part that [share screen](screen-share.md) uses (a watch-only stream to the whole party that needs no invitation and no cloud record, [ADR 0023](../docs/adr/0023-share-screen-is-watch-only-and-asks-the-cloud-for-nothing.md)), and the game's side of share play: the [bridge kinds](bridge-protocol.md#share-play-0x05xx), the [frame ring](#frame-ring), the [pad state](#pad-state) with the host's rules, and the host's [override](#the-hosts-override). Not built: the invitation, the cloud's `Share`, and the share connection that carries the stream and the guest's pad between two PCs.

The decisions are in [ADR 0019](../docs/adr/0019-share-play-streams-through-a-worker.md), the order of work in the [plan](../docs/share-play-plan.md), and the prior art with its sources in the [research notes](../docs/share-play-research.md). The terms are in [CONTEXT.md](../CONTEXT.md). Share play goes through the tunnels like voice ([ADR 0010](../docs/adr/0010-voice-and-shared-play-go-through-the-tunnels.md), [ADR 0017](../docs/adr/0017-voice-has-a-connection-of-its-own.md), [voice spec](voice.md)).

## What it is

The **host** runs a game in AnyPS5 on their PC. They invite one member of their party, the **guest**. The guest sees the game's picture and hears its sound, and depending on the **share mode** may send pad input back. The guest needs neither the game nor its files and gets neither.

| Mode | The guest sees and hears | The guest's pad | The host's pad |
|---|---|---|---|
| `watch` | yes | none | plays as usual |
| `hand_over` | yes | plays as the host's pad | muted for the game |
| `together` | yes | a second pad | plays as usual |

- Only the host starts a share and only the host changes the mode. The guest may ask for a mode; asking changes nothing until the host agrees.
- Both sides agree every time: the host by inviting, the guest by accepting. Nothing is remembered between shares, and nothing starts without a person pressing a button.
- One share per account at a time, with one guest. Several guests in `watch` is a later extension ([open questions](#open-questions)).
- It works only between two members of the same party, and only while the host's game runs.

### What the guest can and cannot do

| The guest can | The guest cannot |
|---|---|
| See what the game shows and hear what it plays | See the host's desktop, other windows, notifications or the launcher |
| In `hand_over` and `together`, press buttons and move sticks of a pad | Send keyboard, mouse, text, or the pad's home button, touch or motion |
| Leave at any moment | Start, stop or change the game, or change any setting of the host |
| Ask for another mode | Read, copy or write any file of the host; no message of the protocol carries a file or the clipboard |
| Talk in the party's voice as before | Get game data, saves, trophy records or tokens: they stay on the host's PC and account |

- Whatever the guest's input does inside the game happens on the host's PC. If the guest saves at an in-game save point, the save is the host's, on the host's PC. Trophies earned during a share are the host's. Playtime is the host's.
- The cloud never sees pictures, sound or pad states. It sees who shares with whom, the title, the mode and the path (direct or relayed).
- The viewer has no record, screenshot or save button. The guest's operating system can still record their own screen. We do not try to stop that, as no gaming service does.

## Parts and where they live

```text
 host PC                                                       guest PC
 ┌──────────┐ ring  ┌────────────┐ link ┌─────────────┐  QUIC  ┌─────────────┐ link ┌────────────┐
 │ AnyPS5   │──────▶│ share      │◀────▶│ net service │◀──────▶│ net service │◀────▶│ share      │
 │ + bridge │◀──┐   │ worker     │      │             │ anyplay│             │      │ worker     │
 └──────────┘   │   │ (encode)   │      └─────────────┘ /share/│             │      │ (decode,   │
   pad states   │   └────────────┘        ▲   │          0     └─────────────┘      │  window,   │
   bridge       └─────────────────────────┘   │                                     │  guest pad)│
                  validated pad states        │ cloud API, events                   └────────────┘
                                              ▼
                                           ┌───────┐              the launcher starts and shows
                                           │ cloud │              all of it (control protocol)
                                           └───────┘
```

| Part | Does | Does not |
|---|---|---|
| **AnyPS5 with the bridge** | Copies each presented picture and the mixed game sound into the frame ring. Takes validated pad states from the bridge as pads. Watches the host's override chord. | Encode, packetise, send, decide who may join |
| **Share worker** (`anyplay-share`) | Host: reads the ring, encodes, packetises, protects against loss, follows the rate it is given. Guest: reassembles, decodes, shows the picture, plays the sound, reads the guest's pad. | Touch the network, the account or the cloud |
| **Net service** | Holds the share connection, checks who connects, validates and rate-limits pad states, measures the path, tells the worker its rate, forwards datagrams, talks to the cloud. | Look into video or sound |
| **Launcher** | Invitation and consent prompts, the host's always-visible indicator and stop button, mode buttons, stats, downloading the worker. | Decode video |
| **Cloud** | The share record, invitations and events, limits, the relay-quota rule. | Carry or store any media |

### Modules and seams

Each module hides a lot behind a small interface, and a seam sits only where two real things can vary.

- **`anyplay-share`, the share engine** (crate `crates/share`, a library): bytes in, bytes out, no clock, no socket, no codec. It cuts encoded frames into datagrams, adds and uses loss protection, reassembles, runs the jitter buffer, runs the rate controller, validates pad states, and reads and writes every wire format in this file. Its interface is a handful of calls: `push_frame`, `poll_datagram`, `on_datagram`, `poll_frame`, `on_report`, `target`. The same crate sits under both ends and under the fuzz targets, as `crates/voice` does for voice ([ADR 0014](../docs/adr/0014-voice-is-an-engine-without-a-transport.md)). Deleting it would spread packetising, loss protection and pad validation over the worker, the net service and the tests.
- **Seam: `VideoEncoder` and `VideoDecoder`** inside the worker. Real adapters from phase 0: one hardware and one test adapter that produces known bytes. More follow (see [codecs](#codecs)).
- **Seam: `FrameSource`** inside the worker: the frame ring, or a test pattern with a frame counter in its pixels. This is how every phase before the engine part is tested.
- **Seam: the frame ring** between engine and worker: the one place the engine's change shows. Its layout is below.
- **Seam: the share link** between net service and worker, a loopback socket with the framing of the [bridge protocol](bridge-protocol.md#frames). A test adapter drops, delays and reorders datagrams on it, which is how loss and relay behaviour are tested in CI without a network.

## Starting, consent and stopping

### Start

1. The host is in a party with the guest and has a game running (`playing` is set, a `game_start` came before).
2. The launcher sends `share_start {account, mode}`. The net service checks that the share worker is there and has an encoder (`share_worker` event), makes its endpoint ready, and asks the cloud: `POST /v1/shares`.
3. The cloud checks the rules in [the cloud API](#cloud-api) and sends the guest a `share_invitation` event. The launcher shows who invites, which game and which mode, with **Accept** and **Decline**. An invitation lives 60 seconds.
4. On Accept, the guest's net service sends its endpoint and relay: `POST /v1/share/accept`. The cloud puts both endpoints in the `Share` and sends the `share` event to both. The smaller endpoint ID dials, as for tunnels and voice.
5. The two net services open the share connection and exchange `hello` on the control stream. The workers announce what they can encode and decode; the host's net service picks the codec and the first rate and tells the worker `start`. The net service sends the bridge `share_open`, and the engine begins to fill the ring.
6. The first picture appears on the guest's screen. The share is `active`.

The guest's viewer opens when they accept. It is a window of its own, started by the launcher, so a crashing decoder never takes the launcher with it.

### Changing the mode

Only the host: `share_mode {mode}`, or a button in the launcher. The host's net service applies it at once, tells the guest on the control stream, and tells the cloud so that the `Share` shows the mode. The host does not wait for the cloud. Leaving `hand_over` or `together` for `watch` removes the guest pad in the engine and sets all its buttons up and its sticks to the middle in the same step. A guest who wants more sends `request {mode}`; the host's launcher shows it and the host answers with `share_mode` or ignores it.

### Stopping at once

| Who and how | What happens |
|---|---|
| The host presses **Stop** in the launcher or in its always-visible indicator | The net service closes the share connection with code 2, tells the bridge `share_close`, tells the cloud `DELETE /v1/share`. |
| The host holds **select and start together for one second** on their pad | The engine tells the bridge, the bridge tells the net service (`override`). In `hand_over` and `together` the mode returns to `watch` at once. In `watch` the share stops. |
| The host presses the **stop key** on the keyboard | As the chord. The key is in the engine's input table beside the screenshot key. |
| The guest leaves, or closes the viewer | Code 3. The host sees a notice. |
| The game ends | The net service sees `game_end` and stops the share. |
| The launcher closes or the net service ends | The connection closes, the cloud ends the share after its grace time. |
| Either side blocks the other, leaves the party, or is banned | The cloud ends the share (`share_gone` with the reason) and both net services close. |
| The connection stalls | After 250 ms without a pad state, the guest pad is neutral. After 5 s without any datagram, the viewer shows that it is waiting. After 60 s, the share ends. |

Stopping is judged by what the guest sees: the picture stops within 250 ms of the host's action, and the guest pad is neutral in the game within 100 ms. The phases test both ([plan](../docs/share-play-plan.md)).

The host must always have a visible sign that a share runs and a way to stop it. The launcher keeps a small, always-on-top indicator with the guest's handle, the mode and **Stop** while a share runs. The engine draws nothing in the first versions ([open questions](#open-questions)).

## Media path

```text
engine present ─▶ copy ─▶ ring ─▶ convert ─▶ encode ─▶ packetise ─▶ link ─▶ QUIC ─▶ link ─▶ reassemble ─▶ decode ─▶ present
 (AnyPS5)                (worker)                      + parity     (net service)                   (guest worker)
```

### Capture in the engine

AnyPS5 already reads the presented picture back to the CPU for screenshots and frame dumps. In `core/libs/prx/libSceAgcDriver/Execution/src/VulkanDevice.cpp`, `present()` records a copy into a host-visible buffer of the frame slot (about line 3569), and `RetireSlot` reads it after the slot's fence (about line 728, `WriteFrameDump` about line 790). The scaler can blit to another size (`PresentationScaler::RecordBlitInto`). Share play uses the same path with a second consumer: a sink that, in place of the picture file writer, copies the buffer into the ring. The cost is one more GPU copy per shared frame and one CPU copy of the picture's bytes.

- **Format and size.** BGRA8, SDR, the size the worker asked for in `share_open`, the game's aspect kept with bars (the scaler already letterboxes). A game that presents HDR is tone-mapped to SDR as for screenshots.
- **Cadence.** Every presented frame up to the stream's frame rate. A game that runs at 20 frames per second is streamed at 20.
- **Never blocks the game.** If the ring is full the engine overwrites the oldest slot. The game's present never waits for the worker.
- **The latency of this route.** The picture is read after the slot's fence, so up to the in-flight depth (`APS5_FLIP_INFLIGHT`, one frame by default) is added to the capture. Phase 1 measures it. If it costs more than the budget, a dedicated fence wait or a GPU-side export (the `gpu` slot kind, below) replaces it.

The tap sits on the presenter thread's submission, so its overhead lands in the game's frame time. The budget is 1.0 ms per frame at 1080p60 on the reference PCs and no more than 5% fewer frames per second ([plan](../docs/share-play-plan.md)).

### Audio in the engine

Game sound reaches the engine through two paths, and the tap needs both. AudioOut2 has a mixer that already makes a stereo 48 kHz float mix (`libSceAudioOut/src/AudioOut2Context.cpp`, `Render` and `context.mix`); the tap copies that mix. The older path (`AudioOut.cpp`, `queueAudio`) has no mixer between ports, so the tap needs a small one: it sums the main and background ports after their volume, resamples to 48 kHz, and skips vibration, controller-speaker, and bitstream ports. Both feed the audio ring as 48 kHz stereo 32-bit float. More than two channels are mixed down in version 0.

### Frame ring

Created by the worker; the engine maps it when the bridge tells it the name. One producer (engine), one consumer (worker), no locks.

| Offset | Size | Field |
|---|---|---|
| 0 | 8 | magic `ANYSHR00` |
| 8 | 4 | layout version, 0 |
| 12 | 4 | slots, 3 |
| 16 | 4 | width |
| 20 | 4 | height |
| 24 | 4 | stride in bytes |
| 28 | 4 | format: 1 BGRA8 |
| 32 | 4 | maximum frames per second |
| 36 | 4 | audio ring size in frames, a power of two (16,384 in version 0) |
| 40 | 8 | audio write position, in frames, written by the engine |
| 48 | 16 | reserved, zero |

The width and the height are even, and at most 3840 by 2160, so that every slot starts on a multiple of 8 bytes, where its `seq` is read atomically. The header is 64 bytes. After the header come the slots, each with a 32-byte slot header and `stride × height` bytes, then the audio ring. A slot header holds `seq: u64` (written last, 0 while the engine writes), `present_us: u64` (the engine's monotonic clock when the picture was presented), `kind: u32` (0 for pixels in the slot; 1 reserved for a GPU handle) and `frame: u32` (the game's flip count). The consumer reads the slot with the highest `seq` it has not seen, copies or uses it, and reads `seq` again; if it changed, the slot was overwritten and the consumer drops the frame. The audio ring is a plain single-producer ring of stereo float frames.

The name is random for each share (`anyplay-ring-` and 32 hex digits) and the mapping is readable only by the user who runs the game; the engine opens it without a namespace prefix ([bridge protocol](bridge-protocol.md#share-play-0x05xx)). On Windows it is a named file mapping in the local namespace; on Linux a `shm_open` object. Nothing else is shared: pad states do not use the ring.

### Codecs

| Codec | Used | Why |
|---|---|---|
| **AV1** | When the host can encode it and the guest can decode it | Royalty-free under the Alliance for Open Media licence; NVIDIA reports about 40% better efficiency than H.264 for its AV1 encoder. Hardware encoders exist on NVIDIA RTX 40 and 50, AMD RDNA 3 and 4, Intel Arc and Core Ultra. Hardware decoders are on most GPUs since 2020, and a software decoder (dav1d) covers the rest. |
| **H.264** | The baseline: always offered | Every GPU decodes it and every GPU encodes it. Constrained High profile, no B-frames, a single slice, low-latency tuning. |
| HEVC | Never | Several patent pools with different terms; AV1 gives the gain. |

- **Picking.** Both workers announce `{codec, hardware, max_width, max_height, max_fps}` for encoding and decoding. The host's net service picks AV1 if the host has a hardware AV1 encoder and the guest has any AV1 decoder; else H.264 with the host's best encoder. A software encoder is used only for H.264, at most 720p30, so that the game keeps its CPU. With no encoder at all, `share_start` fails with `no_encoder` before any invitation goes out.
- **On Windows the codecs are the system's** ([ADR 0027](../docs/adr/0027-video-is-h264-from-the-system.md)): H.264 only, encoded by the first hardware encoder Media Foundation lists (NVENC, AMF and Quick Sync register themselves there), else the system's software encoder (held to 720p30; hardware to 1080p60), and decoded by the system's decoder. The worker contains no codec code. The encoder runs in low-latency mode with constant bit rate, High profile (Main or Baseline where refused), no B-frames, one IDR at the start and on request and a safety IDR every 600 frames, SDR BT.709 limited range; the SPS and PPS travel in front of every IDR. The bit rate follows the rate controller while the encoder runs.
- **On Linux, for now, H.264 in software** (ADR 0027): Cisco's OpenH264 binary, which the worker loads at run time from `ANYPLAY_OPENH264` or beside itself, and only a build whose SHA-256 is known, checking and loading the one file it opened; `ANYPLAY_OPENH264=off` turns it off (the player's switch); it is held to 720p30 like any software encoder and runs with the same settings (constant bit rate, Baseline profile, no B-frames, IDR at the start, on request and every 600 frames, BT.709 limited range), and decodes as well. Without the library the worker reports no codec, so `can_share` and `can_watch` stay false, and says why in `libraries`: `openh264` is `ready`, `off`, `missing`, `refused` (a file that is not a known build, with its SHA-256) or `failed` (a file that could not be read, or a known build that would not load). Clips use only hardware encoders, so Linux has none until a hardware path exists (VA-API for AMD and Intel, NVENC called directly for NVIDIA).
- **Hardware encoders elsewhere** are reached through the vendor runtimes that the driver installs: NVENC (`nvEncodeAPI`), AMF (`amfrt`), Intel Video Processing Library (Quick Sync), VA-API on Linux. The worker loads them at run time; it ships none of them. Their headers and dispatchers are MIT licensed. The first implementation reaches all four through the LGPL build of FFmpeg's libavcodec (shared libraries, no GPL or nonfree parts) behind the `VideoEncoder` seam; direct vendor calls can replace it where measurements say so.
- **Software H.264** is OpenH264, loaded from Cisco's own binary, downloaded to the player's PC by the launcher on first use and switchable by the player, as Cisco's licence terms ask. The worker never contains OpenH264 itself. x264 is GPL and is not used.
- **Decoding** prefers the platform's decoder: DXVA/Media Foundation on Windows, VA-API or Vulkan Video on Linux, dav1d for AV1 in software, OpenH264 for H.264 in software. Some Linux distributions ship their Mesa drivers without H.264 and HEVC hardware decoding for patent reasons; there the viewer falls back to OpenH264 or tells the guest.
- **Patents, honestly.** Using a GPU vendor's encoder does not by itself license the codec's patents for us. AMD's licence says so in its own words, and common practice (OBS, Sunshine, Parsec, Steam) rests on the vendor and platform licenses plus the free personal-use terms of the H.264 pool. Free to end users is not the same as free to a distributor. This is a legal-review item, not a solved one ([clean-line check](#the-clean-line-check)).
- **Settings.** `keyint` is one IDR at start and on request, otherwise never; B-frames off; one reference frame; CBR with a one-frame VBV buffer; low-latency or ultra-low-latency presets; SDR BT.709, limited range.

### Packetising and loss protection

Video travels as QUIC datagrams ([RFC 9221](https://www.rfc-editor.org/rfc/rfc9221)), which the connection's congestion controller governs, and is never retransmitted: a late picture is worse than a lost one. The packetiser cuts each encoded frame into data packets of at most `max_datagram_size - 20` bytes (the size the connection reports, at least 1000), and adds parity packets.

- **Loss protection** is Reed-Solomon over GF(256) across the packets of one frame, a systematic code, so a frame that arrives whole needs no decoding. A group has at most 255 packets, data and parity together; a larger frame is cut into groups of equal size. Parity is `ceil(data × r)` packets with `r` set by the rate controller from the loss it measures: 0 while loss stays below 0.5% over 2 seconds, then rising in steps of 5% to at most 30%. A key frame gets at least 10% parity when `r` is above 0. Reed-Solomon is old, free of patents, and what Moonlight's open protocol also uses; RaptorQ is more efficient but carries patent declarations.
- **A frame that cannot be completed** after its group's parity is used is dropped, and the receiver asks for a key frame (control message `key`) at most once every 500 ms. Until the key frame arrives the viewer shows the last complete picture, never a damaged one. A frame that is still incomplete 100 ms after its first packet is given up, and a complete frame waits at most 20 ms for an older one that is missing, so a late packet delays nothing for long. After a gap only a key frame is shown, and the stream starts with one. After 2 seconds of this it shows that the connection is poor.
- **A frame that waits too long.** On the host, an encoded frame that has waited more than 150 ms in the send queue is discarded and the next frame is a key frame. Capture drops frames, not the encoder's output, while the queue holds more than one frame.
- **Not used in version 0:** reference-frame invalidation and gradual intra refresh. Both are better than key frames after loss and depend on the encoder; phase 4 looks at them.

### Video datagram

All numbers are big-endian.

| Offset | Size | Field | |
|---|---|---|---|
| 0 | 1 | version | 0 |
| 1 | 1 | type | 1 video |
| 2 | 1 | flags | bit 0 key frame, bit 1 parity packet; bits 2 to 7 must be 0 |
| 3 | 1 | epoch | counts stream changes (codec, size); a packet from another epoch than the current is dropped |
| 4 | 4 | frame | counts the frames sent, wrapping |
| 8 | 4 | timestamp | host microseconds at capture (`present_us` of the ring), wrapping at 2^32 |
| 12 | 1 | group | which group of the frame |
| 13 | 1 | groups | number of groups, 1 to 8 |
| 14 | 2 | index | place in the group: data packets first, then parity |
| 16 | 2 | data | data packets in the group, 1 to 254 |
| 18 | 2 | parity | parity packets in the group, 0 to 254, with `data + parity ≤ 255` |
| 20 | 1 to … | payload | bytes of the encoded frame, or parity over them |

The bytes of the encoded frame are preceded by their length as 4 bytes (big-endian), at the start of the first data packet of the first group, so that a receiver knows the exact size even when the short last packet is lost. A receiver refuses, without a reply, a datagram that is shorter than 21 bytes, has another version, a flag bit set that must be 0, `index ≥ data + parity`, `groups` 0 or above 8, or a payload of another length than its group's other packets (the last data packet of a group may be shorter; parity packets have the length of the longest data packet). It keeps at most 4 frames in reassembly and 16 MiB in total; older ones are dropped.

### Audio datagram

| Offset | Size | Field | |
|---|---|---|---|
| 0 | 1 | version | 0 |
| 1 | 1 | type | 2 audio |
| 2 | 1 | flags | bit 0 the stream was interrupted before this packet; others 0 |
| 3 | 1 | epoch | as for video |
| 4 | 2 | sequence | counts audio packets sent, wrapping at 65536 |
| 6 | 4 | timestamp | the sender's 48 kHz sample clock, wrapping |
| 10 | 1 to … | payload | one Opus packet |

One Opus packet ([RFC 6716](https://www.rfc-editor.org/rfc/rfc6716)) of 10 ms, stereo, 48 kHz, music-tuned, in-band forward error correction on, about 96 kbit/s, constrained variable bitrate. It uses `libopus` as the voice engine does ([ADR 0014](../docs/adr/0014-voice-is-an-engine-without-a-transport.md)). Audio packets are sent before the video packets of the same instant. Video is shown as it arrives; audio plays after a buffer of 40 ms that grows with the measured jitter up to 120 ms. If audio and picture drift apart by more than 40 ms the viewer delays the picture to match.

### Echo with party voice

Game sound that the guest's speakers play can be picked up by the guest's microphone and sent to the party, because voice has no echo cancellation ([ADR 0014](../docs/adr/0014-voice-is-an-engine-without-a-transport.md)). Push-to-talk stays the default. While a share runs, the launcher tells the guest to use headphones if voice is in open-microphone mode. The viewer can later hand its playback signal to the voice engine's `AudioIo` as the reference for a canceller.

## Transport

### The share connection

The net services of host and guest hold one QUIC connection with the ALPN `anyplay/share/0` on the tunnels' iroh endpoint, while a share is `active`. It is not a path of its own: direct or relayed, tickets, hole punching and the relay-only setting apply as they do for tunnels and voice. A restart of the tunnels closes it and the dialing side dials again on the new endpoint, which the guest sees as a short freeze.

1. The cloud's `Share` names the host's and the guest's endpoint and relay. That is the only source of who may connect.
2. Of the pair, the smaller endpoint ID dials and the other accepts.
3. A connection counts only if the endpoint the handshake proves is the other party of the `Share` that is `active` (or `invited` for the host waiting for its guest). Anything else is closed at once with code 1 and never reaches the worker. A second connection from the right endpoint replaces the first (code 4).
4. When the cloud tells that the share ended, or its `revision` shows other endpoints, the connection closes with code 2 (the host ended it), 3 (the guest left), 5 (a block or ban) or 4 (replaced).

Close codes: 0 normal end, 1 not the guest, 2 the host stopped, 3 the guest left, 4 replaced, 5 blocked or banned, 6 protocol error, 7 too many invalid pad states, 8 the share timed out.

### Control stream

The dialing side opens one bidirectional stream right after the handshake. Each message is one JSON object on one line, in UTF-8, ended by `\n`, at most 4 KiB. A message the receiver does not know is ignored; a field it does not know is ignored. At most 50 messages a second from the guest; more closes the connection with code 6.

| `msg` | From | Fields | Meaning |
|---|---|---|---|
| `hello` | both | `version`, `role` (`host` or `guest`), `encode`, `decode` | Opens the stream. The lists have `{codec, hardware, max_width, max_height, max_fps}`. |
| `config` | host | `epoch`, `codec`, `width`, `height`, `fps`, `kbps`, `mode` | A new stream setup. Sent at the start and on every change of codec, size or mode. |
| `request` | guest | `mode` | The guest would like this mode. |
| `report` | guest | `frame` (highest frame received), `received`, `lost`, `recovered` (packet counts since the last report), `delay_trend_us`, `queue_ms`, `decoded_fps`, `shown_fps` | Every 100 ms: what the rate controller needs. |
| `key` | guest | `epoch`, `frame` | A key frame is needed from `frame` on. |
| `ping` / `pong` | both | `t0`, `t1`, `t2` | Four timestamps, to measure the round trip and the clocks' difference. |
| `stop` | both | `reason` | The sender is ending the share. |

### Congestion control and the rate

The connection's QUIC controller sets how many bytes may be in flight; datagrams that do not fit are dropped or wait ([RFC 9221](https://www.rfc-editor.org/rfc/rfc9221#section-5.4)). Left alone, a video encoder that ignores it loses datagrams in the network stack instead of in the encoder. So the encoder follows the connection:

- **Inputs.** The guest's `report`s every 100 ms; the connection's own statistics (round trip, lost packets, bytes acknowledged per second, congestion window); the depth of the send queue.
- **Target rate.** `min(player's maximum, path maximum, 0.85 × recent delivery rate)`. The player's maximum is 15,000 kbit/s unless changed. The path maximum is 6,000 kbit/s on a relayed path (the cloud may set another in `GET /v1/info`) and 25,000 on a direct one.
- **Up.** After 1 second without loss above 0.5% and with a flat delay trend, the target rises by 10%. At the start it begins at 4,000 kbit/s and rises by 25% a step until a limit shows.
- **Down.** At once, once per 300 ms, to 80%, when a report shows loss above 2%, a delay trend above 4 ms per 100 ms, or the send queue holds more than one frame.
- **Ladder.** The target picks the size and rate: from 10,000 kbit/s 1080p60, from 5,000 720p60, from 2,500 720p30, below that 540p30; below 1,000 the video pauses and the sound goes on. Down moves after 1 second below a rung; up after 8 seconds above 1.2 times it. Each change starts a new epoch with a key frame.
- All numbers are starting values for phase 0, which tunes them. The design follows the delay-based controllers for real-time media that the IETF has published for exactly this nesting (SCReAM, [RFC 8298](https://www.rfc-editor.org/rfc/rfc8298), over QUIC datagrams), without their packet-level feedback; our feedback is per 100 ms because the control stream is reliable.
- The controller is a pure function of its inputs in `anyplay-share`, tested with recorded traces.

### The relay, cost and privacy

The relay forwards the share's encrypted datagrams as it does tunnel and voice traffic. It cannot tell them apart and sees only sizes and times.

- **Relay-only** (Hide Your IP Address in the launcher, [control protocol](control-protocol.md): `relay_only`) keeps both ends on the relay if either side has it set. Neither learns the other's addresses. The path maximum for relayed shares then applies, and the launcher says so before the invitation: "Relayed: up to 720p60".
- **Cost.** A relayed share at the 6,000 kbit/s cap sends about 2.7 GB an hour through the relay, and the relay pays for that traffic once when it receives and once when it sends. At 1 to 10 dollars per terabyte that is a few cents an hour. About nine in ten connections go direct ([ADR 0002](../docs/adr/0002-tunnels-on-iroh.md)), where the relay carries nothing and the host pays for its own upload.
- **Quota.** Relays check tickets and never call the cloud ([ADR 0007](../docs/adr/0007-relays-check-signed-tickets.md)), and cannot see that traffic is share play. So the hard limit stays what it is: the relay's per-client rate limit, the operator's setting. On top of that the host's net service reports the path of its share (`PATCH /v1/share` with `path`), and the cloud ends a share that has been relayed longer than the operator's daily allowance for the host, 120 minutes by default (`share_gone` with reason `quota`). A modified client can lie about the path; the relay's rate limit is the answer to that.
- **What the cloud sees** is metadata it already has for sessions: accounts, title, mode, times, path kind.

## Input

### Pad state

The guest's viewer reads the guest's pad (or keyboard, mapped to a pad by the viewer) and sends **pad states**. A pad state is exactly this, and nothing else can be sent:

| Field | Size | Values |
|---|---|---|
| `buttons` | 4 | bit 0 `dpad_up`, 1 `dpad_down`, 2 `dpad_left`, 3 `dpad_right`, 4 `south`, 5 `east`, 6 `west`, 7 `north`, 8 `l1`, 9 `r1`, 10 `l3`, 11 `r3`, 12 `start`, 13 `select`, 14 `pad_click`. Bits 15 to 31 must be 0. |
| `lx`, `ly`, `rx`, `ry` | 1 each | 0 to 255, 128 is the middle |
| `l2`, `r2` | 1 each | 0 to 255 |

There is no home button, no touch, no motion, no keyboard and no mouse in version 0. The names are neutral; the engine maps them to the buttons its pad layer knows.

### Pad datagram

| Offset | Size | Field | |
|---|---|---|---|
| 0 | 1 | version | 0 |
| 1 | 1 | type | 4 pad |
| 2 | 2 | sequence | of the newest state, wrapping |
| 4 | 1 | count | 1 to 3 states, newest first |
| 5 | 10 × count | states | `buttons`, `lx`, `ly`, `rx`, `ry`, `l2`, `r2` |

Each datagram repeats the two states before the newest, so one or two lost datagrams cost nothing: a button release is never lost for good. A state is complete, not a change, so the host needs no memory of earlier ones.

### Rules the host's net service applies

1. The datagram must come on the share connection of an `active` share and the mode must be `hand_over` or `together`. In `watch` pad datagrams are dropped without a reply.
2. It must parse exactly: version 0, `count` 1 to 3, the exact length, bits 15 to 31 zero. Anything else is invalid.
3. At most 300 states a second are accepted, with a burst of 30. More are dropped, keeping the newest. The viewer sends at most 250 a second, only when a state changes, plus one every 100 ms so that the host knows it is alive.
4. A sequence at or behind the newest one seen (taking the wrap into account) is dropped. States inside a datagram that were seen already are skipped.
5. More than 20 invalid datagrams in 5 seconds close the connection with code 7.
6. When no valid datagram has come for 250 ms, the host sends the engine a neutral state: no buttons, sticks in the middle, triggers 0, and keeps that until a new datagram arrives.
7. Valid states go to the engine as `share_pad` on the bridge (below). Nothing goes to the operating system.

### The host's override

The host's pad and keyboard are read by the engine itself, so the override cannot be blocked by the guest or by a slow network. The chord (select and start, one second) and the stop key are checked in the engine's pad loop before the guest's state is applied. The engine tells the bridge `override`; the engine also acts on its own: in `hand_over` it stops using the guest pad at once, without waiting for the net service, because the pad loop is where the guest pad enters the game.

### The guest pad in the engine

| Mode | Pad 1 | Pad 2 |
|---|---|---|
| `watch` | the host's | none |
| `hand_over` | the guest's state, in place of the host's | none |
| `together` | the host's | the guest's |

The engine reports the guest pad as an ordinary connected controller on a port, connected when the mode starts and disconnected when it ends, so the game sees a plug and an unplug, as with a real pad. Hand over needs only a second source in the pad state beside the file-based injector (`InjectedInput` in `libScePad/src/PadState.cpp`), because AnyPS5 today knows one pad and one user (`libScePad/Export.cpp`, `libSceUserService/Export.cpp`). `together` needs more: a state per pad handle, a second user for `scePadOpen` and the user service. That is real engine work, the one part of share play that is not small, and the reason the plan puts it last.

Rumble and other output to the pad go back to nobody in version 0. The host's own pad rumbles as before in `together`.

## Latency

Two numbers are measured and shown in the launcher.

- **Stream delay**: from the host engine presenting a picture to the guest's viewer presenting the same picture.
- **Input delay**: from the guest's pad state changing to the host's engine receiving it.

| Stage | Budget (ms) | Where |
|---|---|---|
| Copy and scale in the engine | 1 to 2, plus up to one frame of fence wait | engine |
| Wait in the ring and convert | 1 | worker |
| Encode (hardware) | 6; software H.264 at 720p30 up to 25 | worker |
| Packetise and loss protection | 1 | worker |
| Net service and loopback hops | 0.2 | net service |
| Network, one way | X | the path |
| Reassemble and wait for a late packet | 1 frame or less (8 at 60 frames per second on average) | worker |
| Decode (hardware) | 4 | worker |
| Present | 8 on average at 60 Hz | viewer |

The targets for a direct path at 1080p60 with hardware encoding and decoding:

| | p50 | p95 |
|---|---|---|
| Stream delay | one-way network delay + 40 ms | one-way + 65 ms |
| Input delay | one-way + 3 ms | one-way + 8 ms |
| Stop (guest's picture stops) | 250 ms | 250 ms |

On a relayed path the one-way delay already includes the relay. Encode or decode time above about 15 ms at 60 frames per second is a warning sign; Parsec gives the same figure in its own help.

### How it is measured

1. **In the stream.** Every frame carries the host's timestamp. The `ping` and `pong` messages every second give the round trip and the clocks' difference; the minimum round trip of the last minute is used, and the error is at most half the round trip when the path is not symmetric. The viewer logs the stream delay for every frame it shows; the launcher shows the median and the 95th percentile of the last 10 seconds.
2. **On one machine.** Host and guest run on one PC, so both use one clock and the stream delay is exact. This runs in CI with the fake game, the test pattern and the software encoder, over the shaped link adapter, and bounds everything except the engine, the GPU and the display.
3. **With a camera.** A phone with 240 frames per second films the host's screen and the guest's screen side by side, both showing a running millisecond counter. This is the ground truth for the gates of phases 1 and 2.
4. **By content.** The fake game and the test pattern draw their frame number into the picture. The viewer reads it back and logs gaps, repeats and wrong order.

## Where the engine changes

All of it is behind the bridge and does nothing without a net service that asks for it. There is no new environment variable: the net service starts the tap with `share_open` over the connection the bridge already has ([bridge protocol](bridge-protocol.md)).

| Change | Where in AnyPS5 | Phase |
|---|---|---|
| Frame sink on the existing picture readback | `libSceAgcDriver/Execution/src/VulkanDevice.cpp` (`present`, `RetireSlot`), new `ShareRing` | 1 |
| Audio tap and the small mixer for the legacy path | `libSceAudioOut/src/AudioOut2Context.cpp`, `AudioOut.cpp` | 1 |
| Bridge kinds `0x05xx` | `libSceNetCtl/src/NetBridgeWire.hpp`, `NetBridge.cpp` | 1 |
| The guest pad as a second source for pad 1; the override chord and key | `libScePad/src/PadState.cpp`, `libSceVideoOut/src/PadInput.cpp`, `InputMapping.cpp` | 2 |
| A state per pad handle, a second user | `libScePad/Export.cpp`, `libSceUserService/Export.cpp` | 3 |

Phases 1 and 2 are new files with a few call sites in existing ones, in the order of a thousand lines. This is an estimate and not a measurement.

## Bridge protocol additions

Kinds `0x05xx`, for the [bridge protocol](bridge-protocol.md). Unknown kinds are skipped, so an older engine ignores them and answers `unsupported` to a request.

| Kind | Name | Direction | Payload |
|---|---|---|---|
| `0x0501` | share open | service → bridge, request | `ring: str` (the name), `ring_bytes: u32`, `width: u32`, `height: u32`, `max_fps: u32`, `mode: u8` |
| `0x8501` | share opened | bridge → service | `result: u8` |
| `0x0502` | share close | service → bridge | empty |
| `0x0503` | share mode | service → bridge | `mode: u8` |
| `0x0504` | share pad | service → bridge | `slot: u8`, `sequence: u16`, `buttons: u32`, `lx: u8`, `ly: u8`, `rx: u8`, `ry: u8`, `l2: u8`, `r2: u8` |
| `0x0505` | override | bridge → service | `action: u8` |
| `0x0506` | share status | bridge → service | `frames: u32`, `dropped: u32`, `audio_underruns: u32` |

- `mode` is 0 `watch`, 1 `hand_over`, 2 `together`. `slot` is 0 for pad 1 and 1 for pad 2. `result` is 0 (mapped, copying), 1 (the ring cannot be opened), 2 (a share is running already), 3 (the game does not present through the tap, for example while it shows a movie).
- `action` is 1 (the host took control back: the mode is now `watch`) or 2 (the host asked to stop).
- `share status` is sent once a second while the ring is in use.
- The bridge never accepts `share pad` unless the last `share mode` said `hand_over` or `together`, and ignores a `slot` the mode does not allow. A game that is not running through the bridge has no share.

## The share link

Between the net service and the worker, on `127.0.0.1`, with the framing of the [bridge protocol](bridge-protocol.md#frames) and its own kinds. The launcher passes the worker `ANYPLAY_SHARE_ENDPOINT` and `ANYPLAY_SHARE_TOKEN` as the bridge does; the net service starts the worker only for a share and ends it with the share. The worker has no network of its own and no access to the account. Version 0, internal: it changes with the worker, which the launcher always downloads for the net service's version.

| Kind | Name | Direction | Payload |
|---|---|---|---|
| `0x0001` / `0x0002` | hello / welcome | worker → service / back | version and token; the mode (`host` or `guest`) |
| `0x0010` | capabilities | worker → service | the encoders and decoders it found, and how each codec library it loads stands |
| `0x0011` | start | service → worker | `epoch`, `codec`, `width`, `height`, `fps`, `kbps`, `ring` (host) |
| `0x0012` | rate | service → worker | `kbps`, `fps`, `parity_percent` |
| `0x0013` | key | service → worker | an IDR is needed |
| `0x0014` | datagram out | worker → service | one datagram to send, with its priority |
| `0x0015` | datagram in | service → worker | one datagram received |
| `0x0016` | report | worker → service | what the controller needs (guest) |
| `0x0017` | pad | worker → service | one pad state (guest): 10 bytes, `buttons` as a big-endian `u32`, then `lx`, `ly`, `rx`, `ry`, `l2`, `r2` |
| `0x0018` | stats | worker → service | encode time, decode time, frames shown, dropped |
| `0x0019` | stop | either | the end |
| `0x001C` | ring ready | worker → service | `name`, `bytes`, `width`, `height`, `max_fps`: the worker made the frame ring (host) |
| `0x001A` | key request | worker → service | the viewer needs a key frame from this frame on (`epoch`, `frame`); the service puts it on the control stream as `key` |
| `0x001B` | ended | worker → service | the worker is ending, with a reason in words (the picture source ended, the encoder failed, the window was closed) |

Datagrams of the share are sent with the audio first, then key frames, then other video. The net service never queues more than 2 frames of video.

## Control protocol additions

For the [control protocol](control-protocol.md).

| `do` | Fields | `ok` |
|---|---|---|
| `share_start` | `account`, `mode` | `{share}`: the invitation went out |
| `share_answer` | `id`, `accept` | `{share}` when accepted, else `{}` |
| `share_mode` | `mode` | `{}` (host) |
| `share_request` | `mode` | `{}` (guest asks) |
| `share_stop` | | `{}` |
| `share_quality` | `max_kbps`, `max_height` | `{}` (host) |
| `share_worker` | `path` | `{encoders, decoders}`: where the launcher put the verified worker |

| `event` | Fields | When |
|---|---|---|
| `share` | `share`, `role`, `state`, `link`, `stats` | the share began, changed or ended; stats at most twice a second |
| `share_invitation` | `invitation` | a party member invited the player |
| `share_invitation_gone` | `id` | expired, answered or ended |
| `share_request` | `mode` | the guest asked for a mode |
| `share_worker` | `state` | `missing`, `downloading`, `ready` or `failed`, and what it can encode |

- `share.stats` has `codec`, `width`, `height`, `fps`, `kbps`, `loss_percent`, `stream_delay_ms` (`p50`, `p95`) and `input_delay_ms` (`p50`, `p95`). `link` is `connecting`, `direct` or `relay`.
- New error codes: `no_encoder`, `no_game` (no game runs), `not_in_party`, `busy` (a share exists already), `not_allowed` (the cloud has share play off).
- The launcher never sees tunnel keys, rings or datagrams.

## Cloud API

For the [cloud API](cloud-api.md). A **share** has at most one guest and lives while it is `invited` or `active`.

```text
Share  { id, host, guest, title_id, title, mode, state, host_endpoint, host_relay, guest_endpoint, guest_relay, path, created, revision }
ShareInvitation { id, from, share, title_id, title, mode, expires }
```

`state` is `invited` or `active`. `mode` is `watch`, `hand_over` or `together`. `path` is `direct`, `relay` or null. The endpoints are given only to the two parties, and the invited guest sees `host_endpoint` only after accepting.

| Request | Body | Answer |
|---|---|---|
| `POST /v1/shares` | `{to, mode, endpoint, relay}` | `Share` (host) |
| `GET /v1/share` | | `Share`, or `not_found` |
| `POST /v1/share/accept` | `{endpoint, relay}` | `Share` (the invited guest) |
| `DELETE /v1/share` | | 204: ends, declines or leaves |
| `PATCH /v1/share` | any of `{mode, path}` | `Share` (host) |
| `GET /v1/share/invitations` | | `{invitations}` |

- `POST /v1/shares` answers `forbidden` when the two are not in the same party, or a block lies between them; `not_found` when the host's presence has no activity; `limit` when either account is in a share already; `forbidden` when the cloud has share play off (the control protocol reports it as `not_allowed`); `bad_request` for an `endpoint` that is not 64 lowercase hex characters or a `relay` that is not an `https` URL.
- `PATCH` changes what the `Share` shows. The host's net service has already applied a new mode, and the guest has had it on the control stream; the cloud's copy is for the launcher and the record.
- A share ends when its host or guest calls `DELETE`, when the host's game ends (`playing` set to null), when either leaves the party, when a block is created between them, when either account is banned or deleted, when an invitation is not accepted in 60 seconds, when the quota is used, and when either account has had no event connection for 60 seconds.

Events, for [Events](cloud-api.md#events):

| Type | Fields | Meaning |
|---|---|---|
| `share_invitation` | `invitation` | A party member invited this account. |
| `share_invitation_gone` | `id` | An invitation expired, was declined or its share ended. |
| `share` | `share` | The share this account is in changed. Keep the highest `revision`. |
| `share_gone` | `id`, `reason` | This account is no longer in the share. `reason`: `ended`, `declined`, `left`, `game_ended`, `blocked`, `banned`, `timeout`, `quota`. |

`GET /v1/info` gains `share: {enabled, relay_kbps}`. A cloud that has share play off says `enabled: false` and answers `forbidden` to the requests above; that is the default for the public cloud until the legal review in the ADR is done.

| What | Limit |
|---|---|
| `POST /v1/shares` | 10 per account per hour |
| Relayed minutes per host and day | 120 (the operator's setting) |
| Time of one share | no limit; the operator may set one |

## Moderation and privacy

- Share play is party-only and one-to-one. There is no link, no public stream and no spectator from outside the party.
- Everything a player can send or show another can be reported, blocked and removed ([SOCIAL.md](../docs/SOCIAL.md)). A report names the account; the cloud holds no video of the share, so there is nothing for a moderator to view. A block ends the share at once.
- The picture comes from the game's own presented frame, so no other window, notification or desktop content can leak into the stream.
- A new account follows the same rule as for parties: it cannot invite strangers, because only party members can be invited.
- The invitation names the title and mode before the guest accepts. A guest who has declined an account's invitations can block it.

## Security

| Threat | Answer |
|---|---|
| A guest sends pad states too fast or malformed | The rules above: exact parsing, 300 per second, closed after 20 invalid in 5 seconds. The parser is fuzzed like the other wire formats ([fuzz](../fuzz/README.md)). |
| A guest tries to reach the host's PC | There is no message for it. Input is a pad inside the game; no keyboard, mouse, clipboard or file message exists. |
| Someone else connects | The handshake proves the endpoint; it must be the guest named by the cloud's `active` share. |
| A host's stream attacks the guest's decoder | The viewer is a separate process without network or file access, decoding is by platform decoders where possible, and the guest accepts only from a party member after a prompt. The packet parsers and the reassembler are fuzzed. |
| A relay or network observer | The connection is end-to-end encrypted by iroh/QUIC. A relay sees sizes and times. |
| Flooding memory | Reassembly holds at most 4 frames and 16 MiB; control messages are 4 KiB and 50 a second; one share per account. |
| Stuck buttons after a drop | A neutral state after 250 ms of silence; the guest pad disappears with the mode. |
| The worker crashes | The engine keeps running (the ring is overwritten, nothing waits); the net service ends the share and tells the launcher. |
| A swapped worker | The launcher checks the download against the release's hash before it tells the net service the path ([supply chain](../docs/supply-chain.md)). |

## The line check

[ADR 0021](../docs/adr/0021-the-line-is-drawn-at-the-law.md) and the [ground rules](../docs/DESIGN.md#ground-rules), one by one.

| Rule | Share play |
|---|---|
| No contact with a platform's servers | None. Everything runs between players and our cloud. |
| No material from a platform (code, keys, firmware, documents) | None. The wire formats here are our own. The research is from public pages and papers; no code was taken from Moonlight, Sunshine or any other program. |
| No tokens for others, no posing as a console | None issued. |
| Formats from observation | Not applicable: these are our own formats, as for voice. |
| No platform trademarks in names | The terms are plain words. The feature's name, share play, is also the name of a console's feature; see [open questions](#open-questions). The pad buttons are named by position, not by a console's symbols. |
| No stand-in for a running publisher service, no bypass of anti-cheat or copy protection | Share play does not touch them. A game whose online play needs a live publisher service stays offline on the PlayStation side with or without a share. |
| Only games the player owns | The game runs only on the host's PC, on the host's copy. The guest gets a picture and sound, as if they watched over the host's shoulder, and never the game or its data. ADR 0021 puts play together by streaming in, with this review before the public launch. **This is where the risk lies.** In `hand_over` and `together` a guest plays a game they do not own. The console feature this is modeled on says in its public help that both players enjoy the game "regardless of who owns it", so the idea is established, but "established" is not "licensed", and a rights holder could see a guest's play differently. The limits that keep this small: one guest, only friends in the host's party, the host present and able to take control back at once, no recording by us, no public stream, no storage. They do not replace a legal review. |
| Compatibility layer, never an emulator | The tap reads the picture AnyPS5 presents and the sound it plays, and gives it a pad. It decrypts and unlocks nothing. |

Two more points that are not on the list. The hardware encoders give no patent license of their own (see [codecs](#codecs)). And the cloud's `share.enabled` is false by default, so that a public cloud does not offer share play before the review.

## Not in version 0

- More than one guest, spectators outside the party, or a public stream.
- Keyboard, mouse, touch, motion, the home button, text and clipboard from the guest.
- Rumble or other output back to the guest's pad.
- HEVC, 4K, HDR, or more than two channels of sound.
- Recording, screenshots or clips by the viewer.
- Reference-frame invalidation, gradual intra refresh, a GPU-side ring.
- An overlay drawn by the engine.
- macOS: the net service and launcher run on Windows and Linux.

## Open questions

1. **The name.** "Share Play" is also the name of a console's feature. The repo's term is lower-case and generic, but the player-facing name should be our own before a public release.
2. **Legal review.** Patents for H.264 and AV1 through hardware and software, and the guest-without-the-game question under the rule about owned games. Until it is done, `share.enabled` is false on the public cloud, as revival servers and the online-permission answer wait for the same review ([ADR 0021](../docs/adr/0021-the-line-is-drawn-at-the-law.md)).
3. **Datagrams or a stream per frame.** Phase 0 measures how the QUIC datagram path and its controller behave at 6 to 15 Mbit/s over a relay. If it does not hold up, a stream per frame with a reset when late is the fallback (ADR 0019).
4. **Where the quinn controller stands.** The default controller of the QUIC stack under iroh, and whether iroh lets us choose BBR or a delay-based one, is not verified here. Phase 0 reads it and measures it.
5. **Capture latency.** The route through the existing readback adds up to one frame. Phase 1 measures it; a GPU-side export may be needed.
6. **HDR games.** Tone-mapping to SDR is what screenshots do. Whether that looks good enough for a stream is not tested.
7. **`together` in the engine.** One pad and one user are built in. How much of that is spread over libraries is only known when someone changes it; phase 3 starts with a spike.
8. **The host's indicator.** The launcher's always-on-top pill does not show over exclusive fullscreen. An engine-drawn badge would, at the cost of engine UI. The chord and the key do not depend on it.
9. **More guests.** Several watchers multiply the host's upload, or the relay's. A limit of 3 in `watch` is plausible and untested.
10. **The chord and the key.** Select and start for one second, and the key, should be checked against the games that use them. The values are settings of the engine.
11. **Echo.** Open microphone plus speakers is bad for voice already. A canceller fed by the viewer's playback is the fix and is not built.
12. **Relay costs.** The numbers here are estimates from list prices; the first public cloud will show the real ones.
