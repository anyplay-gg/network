# Known gaps

What the network does not do yet, part by part. Each line says what is missing and what closes it. See [OBSERVING.md](OBSERVING.md) for how formats are learned.

## Bridge (anyplay-gg/AnyPS5)

- **Sign-in, identity, NAT and web API forwarding**: drafted on `feat/net-bridge`, under review (board task T2).
- **Push event delivery**: missing. The signature of the game's push callback is not known. Closes with an observation of a game that registers one.
- **Session signaling, peer-to-peer sockets, join intents and the invitation dialog**: missing in the bridge. The net service already serves them ([bridge protocol](../spec/bridge-protocol.md) groups `0x03xx` and `0x04xx`).
- **Online permission**: the bridge still leaves the check as AnyPS5 answers it today. The target is the network's own rule, signed in and not banned, for the AnyPlay network only ([ADR 0021](adr/0021-the-line-is-drawn-at-the-law.md)). Closes with the bridge change and a lawyer's review before the public launch.

## Net service (this repository)

- **Revival servers**: none built. [ADR 0021](adr/0021-the-line-is-drawn-at-the-law.md) allows them for games whose publisher has shut the service down. Each one needs observation of a game the player owns, and a lawyer's review before the public launch.
- **Web API groups**: none of the platform's is offered yet; every request is answered "not offered" and written to the observation log. Only the network's own group `anyplay` is answered ([spec](../spec/own-api-group.md)). Closes group by group, from observations of class A titles; what each group sends is written down in [spec/web-api](../spec/web-api/README.md), starting with `userProfile`.
- **Push events to the game**: not sent until the bridge can deliver them.
- **NAT type**: reported as 2 while a relay is reachable, 0 otherwise; no finer measurement.
- **Voice chat**: the engine ([`crates/voice`](../crates/voice), [ADR 0014](adr/0014-voice-is-an-engine-without-a-transport.md)) is wired into the net service: voice connections between party members, membership checks, the control commands and events ([spec](../spec/voice.md), [DESIGN](DESIGN.md)). Tested without hardware, on memory devices and real iroh endpoints with a relay on loopback. Still missing:
  - the cloud's side of `PUT /v1/party/endpoint` and the new `PartyMember` fields (the net service tolerates a cloud without them: nobody is dialed), and the launcher's party window;
  - a run on real microphones and speakers, on Windows and on Linux, and measurements of delay and loss on real paths; the `devices` build is only compiled in CI;
  - no echo cancellation: push-to-talk is the default, and an open microphone needs headphones;
  - one account on two devices in the same party: the second device's announcement replaces the first's, and the first loses voice;
  - the pump runs on a Tokio timer, which on Windows can tick only every 15 ms or so rather than every 5 ms; the engine's queue allows for that, but a thread of its own would be steadier;
  - no gain or level meter before a talk, no per-device sample-rate choice (the device's default is used);
  - phase S3 is done when a party of four talks with relay only switched on, which needs the cloud and a real run.
- **Share play**: designed ([spec](../spec/share-play.md), [plan](share-play-plan.md)), nothing built. Needs a prototype of video over datagrams on real networks (phase 0), a tap in the engine (frame, sound, pad), the cloud's share records, and a legal review before a public cloud turns it on.
- **Matchmaking, leaderboards, cloud saves**: not started (phase 5).
- **Fuzzing beyond the parsers**: the parsers of both protocols are fuzzed every night ([fuzz](../fuzz/README.md)); what the net service does with the messages they read, and what it reads from the cloud, is tested with fixed cases only.

## Cloud (anyplay-gg/cloud)

- **News between nodes**: presence, sessions and invitations are in the store, which every node of a cloud shares, but the nodes do not tell each other about changes yet, so a cloud runs as one node. Closes with stage 2 of the [cloud architecture](https://github.com/anyplay-gg/cloud/blob/main/docs/ARCHITECTURE.md) (NATS).
- **Parental consent**: players under the age of consent cannot sign up.
- **Sign-in with Apple, Google or Discord**: not offered; e-mail code and passkey, in the browser ([ADR 0016](adr/0016-accounts-are-managed-on-the-web.md), which says where it would fit).

## Launcher (anyplay-gg/launcher)

- **No default cloud**: until AnyPlay's own cloud runs, release builds name none and the player enters one.
- **Game launch on Linux** with the bridge is untested.
- **Signed releases**: the binaries carry no code signature, and the self-update checks a download only against the checksums of the same release. Closes with a signing key kept outside GitHub, whose public half ships in the launcher.
