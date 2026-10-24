# Known gaps

What the network does not do yet, part by part. Each line says what is missing and what closes it. See [OBSERVING.md](OBSERVING.md) for how formats are learned.

## Bridge (anyplay-gg/AnyPS5)

- **Sign-in, identity, NAT and web API forwarding**: drafted on `feat/net-bridge`, under review (board task T2).
- **Push event delivery**: missing. The signature of the game's push callback is not known. Closes with an observation of a game that registers one.
- **Session signaling, peer-to-peer sockets, join intents and the invitation dialog**: missing in the bridge. The net service already serves them ([bridge protocol](../spec/bridge-protocol.md) groups `0x03xx` and `0x04xx`).
- **Online permission**: waits for phase 4 ([ADR 0008](adr/0008-the-online-permission-check-waits.md)).

## Net service (this repository)

- **Web API groups**: none of the platform's is offered yet; every request is answered "not offered" and written to the observation log. Only the network's own group `anyplay` is answered ([spec](../spec/own-api-group.md)). Closes group by group, from observations of class A titles; what each group sends is written down in [spec/web-api](../spec/web-api/README.md), starting with `userProfile`.
- **Push events to the game**: not sent until the bridge can deliver them.
- **NAT type**: reported as 2 while a relay is reachable, 0 otherwise; no finer measurement.
- **Voice chat, matchmaking, leaderboards, cloud saves**: not started (phase 5).
- **Fuzzing beyond the parsers**: the parsers of both protocols are fuzzed every night ([fuzz](../fuzz/README.md)); what the net service does with the messages they read, and what it reads from the cloud, is tested with fixed cases only.

## Cloud (anyplay-gg/cloud)

- **One instance**: presence, sessions and invitations live in the memory of one cloud process. Several instances need a shared bus first.
- **Parental consent**: players under the age of consent cannot sign up.
- **Sign-in by passkey**: not offered; e-mail codes only ([ADR 0005](adr/0005-sign-in-by-e-mail-code.md)).

## Launcher (anyplay-gg/launcher)

- **No default cloud**: until AnyPlay's own cloud runs, release builds name none and the player enters one.
- **Game launch on Linux** with the bridge is untested.
- **Signed releases**: the binaries carry no code signature, and the self-update checks a download only against the checksums of the same release. Closes with a signing key kept outside GitHub, whose public half ships in the launcher.
