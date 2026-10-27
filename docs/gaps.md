# Known gaps

What the network does not do yet, part by part. Each line says what is missing and what closes it. See [OBSERVING.md](OBSERVING.md) for how formats are learned.

## Bridge (anyplaynet/AnyPS5)

Merged in the fork: sign-in, identity, NAT type, web API forwarding, push events ([#7](https://github.com/anyplaynet/AnyPS5/pull/7)), frame tests ([#10](https://github.com/anyplaynet/AnyPS5/pull/10)), session signaling and peer-to-peer datagrams ([#11](https://github.com/anyplaynet/AnyPS5/pull/11)), peer-to-peer streams ([#12](https://github.com/anyplaynet/AnyPS5/pull/12)) and the online-permission check ([#24](https://github.com/anyplaynet/AnyPS5/pull/24)).

- **End-to-end CI**: the test that runs the bridge against `anyplay-net` is in review ([#9](https://github.com/anyplaynet/AnyPS5/pull/9)).
- **Join intents and the invitation dialog**: not in the bridge. The net service already serves them ([bridge protocol](../spec/bridge-protocol.md) group `0x04xx`).
- **Push events to a real game**: delivery is merged and tested with frames. Whether a game accepts what the net service pushes is learned from observation of games that register a callback.
- **Online permission**: the bridge answers by the network's own rule, signed in and not banned, for the AnyPlay network only ([ADR 0021](adr/0021-the-line-is-drawn-at-the-law.md)). Needs a lawyer's review before the public launch.
- **A game played together**: no game has played with another over the internet yet. Phase 4 of the [roadmap](DESIGN.md#roadmap) is open.

## Net service (this repository)

- **Revival servers**: none built. [ADR 0021](adr/0021-the-line-is-drawn-at-the-law.md) allows them for games whose publisher has shut the service down. Each one needs observation of a game the player owns, and a lawyer's review before the public launch.
- **Web API groups**: the net service answers the network's own group `anyplay` ([spec](../spec/own-api-group.md)), `userProfile` (friends and profiles from the AnyPlay friend list) and `communicationRestrictionStatus`. The body shapes of the last two are guesses until a game we own accepts them. Every other group is answered "not offered" and written to the observation log. Groups close one by one from observations; what each sends is in [spec/web-api](../spec/web-api/README.md).
- **NAT type**: measured from iroh's net report ([bridge protocol](../spec/bridge-protocol.md)). Whether a NAT shows one address to every server needs two relays that answer probes; with one it is unknown. A port mapping on the router (UPnP, PCP) does not make a PC type 1. Only IPv4 counts. The probes test outgoing UDP and its answer: a PC with a public address whose firewall drops unasked incoming UDP (Windows on a public network profile) is still reported as 1.
- **Voice chat**: the engine ([`crates/voice`](../crates/voice), [ADR 0014](adr/0014-voice-is-an-engine-without-a-transport.md)) is wired into the net service: voice connections between party members, membership checks, the control commands and events ([spec](../spec/voice.md), [DESIGN](DESIGN.md)). Tested without hardware, on memory devices and real iroh endpoints with a relay on loopback. Parties with voice run in the launcher. Still missing:
  - measurements of delay and loss on real paths and on a range of real microphones and speakers; the tests use memory devices;
  - no echo cancellation: push-to-talk is the default, and an open microphone needs headphones (noise suppression can be switched on);
  - one account on two devices in the same party: the second device's announcement replaces the first's, and the first loses voice;
  - the pump runs on a Tokio timer, which on Windows can tick only every 15 ms or so rather than every 5 ms; the engine's queue allows for that, but a thread of its own would be steadier;
  - no gain or level meter before a talk, no per-device sample-rate choice (the device's default is used);
  - phase S3 is done when a party of four talks with relay only switched on, which needs a real run.
- **Share play**: built in the net service, the cloud and the worker, and tested with fakes only ([spec](../spec/share-play.md), [plan](share-play-plan.md)): the invitation, the share connection, the modes, the stream and input delay, `share_quality`. Missing: the engine side in a real title (the frame ring, the guest's pad and a second user are open in the fork, anyplaynet/AnyPS5#29 and #30), the launcher's share window (the launcher keeps share play hidden), and a legal review before a public cloud turns it on. The numbers in the plan have not been measured between two real networks.
- **Share screen sound**: Windows and macOS. On a Mac the streamer hears the game's applications through ScreenCaptureKit, and a game whose processes the system does not list as an application has no sound; a Mac has no viewer window yet, so it cannot watch. On Windows capture needs Windows 10 build 20348 or later (process loopback); on an older one the stream has no sound and the picture goes on. It is sent from a thread of its own, not strictly before the video of the same instant, and the viewer does not hold the picture back when the two drift apart. The viewer decodes the sound on the window's loop, 20 ms ahead of the speakers, so anything that stops that loop for longer (Windows' own loop while the window is dragged or resized) is heard as a gap. In-band forward error correction at the expected 10% loss makes Opus code the band below 8 kHz with its speech coder (hybrid mode) instead of the music coder: a lost packet is rebuilt better than concealment can (9 dB against -1 dB on music), at a cost to every packet's quality; the trade-off is not measured by ear yet. The capture and the speakers are compiled and linted for Windows, but no test plays a game's sound: the tests use a tone and a clock in place of the speakers.
- **Share screen on Linux**: the worker captures the window the player picks in the portal and encodes it on the GPU through NVENC (NVIDIA) or VA-API (Intel, AMD), loaded from the system and probed at start ([ADR 0037](adr/0037-linux-encodes-on-the-gpu-through-vaapi-and-nvenc.md)), else with Cisco's OpenH264 in software (720p30). Clips work on Linux with the same hardware encoders. The two GPU paths are tested with fakes and by a round trip that skips without a GPU: they have not run on real hardware in CI, and the first run on each vendor's GPU is still to be done. Clips ask the player for the game's window in the portal dialog each time a game starts. Missing: game sound on Linux, at either end; hardware decoding. The worker links the system's `libpipewire-0.3` and does not start without it, so a Linux PC without PipeWire can neither share nor watch.
- **Matchmaking, leaderboards, cloud saves**: not started (phase 5).
- **Bridge protocol 1** ([ADR 0039](adr/0039-the-bridge-proves-the-token-instead-of-sending-it.md)): the HMAC challenge is merged on both sides (here #125, in the fork #31). Protocol 0 is no longer spoken, so a bridge built before it must be updated.
- **Fuzzing beyond the parsers**: the parsers of both protocols are fuzzed every night ([fuzz](../fuzz/README.md)); what the net service does with the messages they read, and what it reads from the cloud, is tested with fixed cases only.

## Cloud (anyplaynet/cloud)

- **News between nodes**: presence, sessions and invitations are in the store, which every node of a cloud shares, but the nodes do not tell each other about changes yet, so a cloud runs as one node (anyplaynet.com does). Closes with stage 2 of the [cloud architecture](https://github.com/anyplaynet/cloud/blob/main/docs/ARCHITECTURE.md) (NATS).
- **Parental consent**: players under the age of consent cannot sign up.
- **Sign-in with Apple, Google or Discord**: not offered; e-mail code and passkey, in the browser ([ADR 0016](adr/0016-accounts-are-managed-on-the-web.md), which says where it would fit).

## Launcher (anyplaynet/launcher)

- **Other clouds**: release builds use AnyPlay's cloud at anyplaynet.com. Pointing the launcher at a self-hosted cloud stays possible.
- **Game launch on Linux** with the bridge is untested.
- **Code signing of the Windows exe**: the exe and installer are not Authenticode-signed, so SmartScreen warns on a first download. The self-update is signed since 0.2.0 ([launcher UPDATES.md](https://github.com/anyplaynet/launcher/blob/main/docs/UPDATES.md)).
