# Share play: plan

The order of work for [share play](../spec/share-play.md), decided in [ADR 0019](adr/0019-share-play-streams-through-a-worker.md). It fills phase S5 of [SOCIAL.md](SOCIAL.md). Each phase ends with numbers that someone can measure; a phase is not done until they are met and written down.

Rules for every phase:

- Runs of real games happen only on test PCs, when the project owner gives the go. Everything else runs on the fake game and the test pattern, in CI.
- Each wire format gets a fuzz target before it gets a user ([fuzz](../fuzz/README.md)).
- `share.enabled` stays false on the public cloud until the legal review in the ADR is done. The phases build and test it anyway.
- Sizes below (S, M, L) are for ordering work, not promises: S is days, M is a couple of weeks, L is a month or more for one person.

Needs from earlier phases: parties and the party's messages (S2, done), the tunnels' endpoint with a second ALPN (done for voice), the launcher's online area (phase 1 of the network), and for phases 1 to 3 the bridge in an AnyPlay build of AnyPS5.

Share screen ([ADR 0023](adr/0023-share-screen-is-watch-only-and-asks-the-cloud-for-nothing.md), [spec](../spec/screen-share.md)) takes the media path of phases 0 and 1 first, to the whole party and without the cloud's `Share`: `crates/share`, the net service's screen hub, the share link and the worker's test pattern, and then window capture in place of the engine tap. The phases below still hold for share play's own parts (the invitation, the cloud record, the pad), and their numbers measure the same pipeline.

## Phase 0: a prototype without the engine

**Goal.** Find out whether video over QUIC datagrams between two real networks is good enough, before any engine, cloud or launcher work. Pick the encoder route.

**Work.**

| Where | What | Size |
|---|---|---|
| `crates/share` | Wire formats (video, audio, pad, control), packetiser, Reed-Solomon, reassembler, jitter buffer, rate controller, pad validator. Pure code with unit tests and fuzz targets. | M |
| `crates/share-worker` | The worker: `FrameSource` (test pattern with the frame number in its pixels), `VideoEncoder` through FFmpeg's LGPL libavcodec (NVENC, AMF, Quick Sync, VA-API) and OpenH264, `VideoDecoder` and a window for the viewer. | L |
| `crates/net` | The `anyplay/share/0` ALPN and a sink beside the voice one (`Tunnels::set_share_sink`), the share link, a `--share-demo host|guest` mode that takes the other side's endpoint on the command line. No cloud. | M |
| `crates/probe` | Shaped link adapter (loss, delay, reorder, rate limit) for CI; real-network runs through the probe's existing setup. | S |
| `docs` | Results: encoder latency per vendor, datagram goodput, controller behaviour. | S |

**Exit criteria.**

1. On one PC, both ends, 1080p60, hardware H.264: stream delay p50 at most 45 ms and p95 at most 70 ms (exact, one clock).
2. Between two PCs on two real networks, direct: stream delay p50 at most one-way delay + 40 ms and p95 at most one-way + 65 ms over 10 minutes at 12 Mbit/s.
3. Under 2% random loss and a 40 ms delay (shaped link, CI): at least 55 frames per second shown, no freeze longer than 500 ms, no damaged picture shown (the frame counter in the pixels is always right).
4. Relay-only, 720p30 at 4 Mbit/s: runs 10 minutes without a stall longer than 1 second.
5. A pad state sent at 250 per second over the same link: input delay p95 at most one-way + 8 ms; no state accepted that fails the rules; fuzzing runs one hour without a crash.
6. The decisions it exists to make are written down: the QUIC controller in use and whether it can be set; whether datagrams or a stream per frame (fallback in the ADR) carry video over a relay; FFmpeg or direct vendor calls for each vendor; the first values of the rate controller.

**Decision gate.** If criterion 4 fails with datagrams, switch to the stream-per-frame design and amend the ADR before phase 1.

**Status (27.10.2026).** Built and tested without a game: the bridge kinds `0x0501` to `0x0506` in `crates/bridge-protocol`; the frame ring (`anyplay_share::ring`, the share worker's `shm` and `--source ring`); the guest's pad state, datagram, sender and the host's gate (`anyplay_share::pad`); `anyplay_net::shareplay::ShareHost`, the host's side towards the game, with a fake game that follows the engine's rules; the viewer's keyboard pad and the first connected gamepad (`anyplay_share_worker::gamepad`, merged with the keyboard; the real device is proven only on a PC with a pad). In the AnyPS5 fork: the bridge kinds, the ring writer, the guest pad in the pad layer, the override chord and the frame sink (a separate pull request). Not built: the invitation and the cloud's `Share`, the share connection (`anyplay/share/0`) that carries the datagrams, the launcher's share window, and the engine-side proof with a real title.

## Phase 1: watch only

**Goal.** A friend without the game watches a game running in AnyPS5.

**Work.**

| Where | What | Size |
|---|---|---|
| AnyPS5 | Frame sink on the picture readback; audio tap with the small mixer; bridge kinds `0x0501` to `0x0503` and `0x0506`; the ring reader's counterpart. Tests against the fake net service. | M |
| `crates/net` | Share state in the service task, the share connection with the member checks, the worker's supervision, `share` events and commands, the bridge side of the ring messages. | L |
| Cloud | `Share`, invitations, events, limits, `info.share`, the allowance rule. | M |
| Launcher | Party window: invite, accept, the host's always-on-top indicator with Stop, the viewer's window, worker download with hash check, stats. | L |
| `crates/share-worker` | AV1 through hardware and dav1d; the audio path; epoch and ladder changes. | M |

**Exit criteria.**

1. A friend on another network watches 30 minutes of a title that the friend does not own, with no crash of the game, the net service, the worker or the launcher.
2. Stream delay (measured with the camera) p50 at most one-way + 45 ms and p95 at most one-way + 75 ms, direct, 1080p60. This includes the engine's copy; if it misses by more than 10 ms because of the fence wait, a dedicated wait or the GPU-side export is built before the phase closes.
3. The engine tap costs at most 1.0 ms of frame time at 1080p60 on the reference PC, and the game loses at most 5% of its frame rate with the tap on, compared with off. The game's present never waits for the worker (a stopped worker changes nothing in the game's frame time).
4. Sound and picture within 40 ms of each other at the guest; no audio gap longer than 100 ms in 30 minutes direct.
5. Stop: the guest's picture stops within 250 ms of the host's action, for each way in the stopping table, and the ring and worker are gone within 2 seconds.
6. The guest receives nothing but the stream: a packet capture of the guest's side shows the share connection and nothing from the host beyond it; the message list in the spec is complete.
7. Relayed: with relay-only on one side, 720p60 at 6 Mbit/s for 30 minutes, with the launcher showing the cap beforehand.
8. HDR: a game that presents HDR is shown in SDR without a crash; the picture quality is judged and written down.

## Phase 2: hand over the controller

**Goal.** The guest plays; the host can take it back.

**Work.**

| Where | What | Size |
|---|---|---|
| AnyPS5 | `share_pad` and `share mode` kinds; the guest's state as a second source for pad 1; the host pad muted in `hand_over`; the chord and the key; neutral on a mode change. | M |
| `crates/net` | Pad rules (parse, rate, order, neutral on silence, close on abuse), `override`, mode changes. | M |
| `crates/share-worker` | The viewer reads the guest's pad (and a keyboard map) and sends states. | S |
| Launcher | Mode buttons, the guest's ask for a mode, the host's prompt. | S |

**Exit criteria.**

1. Input delay (guest's state changed to the host's engine receiving it, logged by the fake game and by a real title): p50 at most one-way + 3 ms, p95 at most one-way + 8 ms, direct.
2. The override: from the chord held for one second to the game ignoring the guest's pad, at most 100 ms; from the stop key, at most 100 ms to the picture stopping at the guest.
3. Silence: kill the guest's net service during a held button; the game sees a neutral pad within 250 ms, every time in 50 tries.
4. Abuse: a client that sends malformed and flooded pad datagrams for 10 minutes gets closed with code 7; no invalid state reaches the engine (counted in the engine's status message); the host's frame time does not move by more than 0.2 ms.
5. A real title is played for 20 minutes by a guest through `hand_over` with no stuck button and no crash. The host is able to take control back at any point during those 20 minutes.
6. A security review of the datagram parsers and the bridge kinds, with the fuzz targets, finds nothing open.

## Phase 3: play together

**Goal.** The guest is a second player.

**Work.**

| Where | What | Size |
|---|---|---|
| AnyPS5 | Spike first: how many libraries assume one pad and one user, written up. Then a state per pad handle, a second user, connect and disconnect of the guest pad. | L |
| Bridge, net service | `slot` 1, mode `together`, nothing else. | S |
| Launcher | The mode, and a note when a title is known to take one pad only. | S |

**Exit criteria.**

1. The spike's write-up is in the repo and the phase is re-sized before building starts.
2. A title with local two-player play runs for 20 minutes with the guest as player 2, direct and relayed, with no crash and no stuck input.
3. The guest pad is connected and disconnected 50 times (mode switches and stops) and the game keeps running every time.
4. The host's own pad is unaffected: the same input delay at the host as with the share off, within 1 ms.

## Phase 4: polish and extensions

Each item only if measurements ask for it.

- A GPU-side ring (the `gpu` slot kind) to remove the CPU copy, if the engine's tap costs more than the budget at 1440p or 4K.
- Reference-frame invalidation or gradual intra refresh, if key frames after loss show up as freezes in the field.
- Rumble back to the guest.
- Several watchers in `watch` (the cost is the host's upload or the relay's).
- An echo canceller for voice that uses the viewer's playback as its reference.
- An engine-drawn badge for the host.
- HDR to HDR, 4K, and more channels of sound.
- Direct vendor encoders in place of FFmpeg, where the measurements favour it.

## Risks, in the order they matter

1. **Legal.** Patents of the codecs and the guest-without-the-game question. The mitigation is a switch that is off, a review before it is on, and a design that keeps the guest to a picture, a sound and a pad. It cannot be solved by engineering.
2. **Datagram video over a relay.** If the path is poor, phase 0 shows it early and the fallback costs a redesign of one module.
3. **Engine work for `together`.** One pad and one user are assumed in several libraries. The spike is the first task of the phase for that reason.
4. **Latency of the capture route.** Up to a frame more than a desktop-capture product. Measured in phase 1; two fixes are ready.
5. **Hardware variety.** Four encoder APIs, three vendors, laptops with two GPUs. The worker reports what it found and the net service can say no before an invitation.
6. **A slow game.** Many games run slowly on AnyPS5 today. The stream follows the game's frame rate and the tap may add to the load; the 5% budget is checked on the slowest reference title.
7. **Unstable test PCs.** Real-game runs wait for the owner's go. The CI parts (fake game, shaped link, loopback) carry most of the criteria.
8. **Echo and open microphones** in parties with voice on.
