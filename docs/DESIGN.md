# Design

How the AnyPlay network lets players of AnyPS5 games play together over the internet. The terms are defined in [CONTEXT.md](../CONTEXT.md).

## The idea

AnyPS5 already replaces the console's system libraries with its own code. So the line between a game and the network is a function call in AnyPS5, not encrypted traffic on the wire. The network plugs in at that call. It needs no DNS redirection, no TLS interception, no patched game code and no contact with Sony.

The same pattern has carried [RPCN](https://github.com/RipleyTom/rpcn) for PS3 games since 2019, and shadNet for PS4 games since 2026. Nobody has built it yet for the session and signaling services that PS5 games use.

## Ground rules

These hold for every line of code and every public statement. [ADR 0021](adr/0021-the-line-is-drawn-at-the-law.md) gives the reasons: the network goes as far as the law allows.

1. The network never contacts Sony's servers. A request the network does not know gets "not available"; it is never forwarded.
2. No material from Sony: no leaked SDKs or documents, no code, firmware, keys, certificates or tickets, no responses recorded from Sony's servers.
3. No tokens for others. When a game asks for an authorization code or ID token for a platform's or a publisher's client, the network answers with an error. It only issues tokens for itself, and it never presents itself to a service as a real console.
4. Formats come from observation. The bridge sees in plain text what a game sends; what the game accepts as an answer is learned from its behaviour. The network's own specifications in [spec/](../spec) are written from that, separately from the code.
5. No platform trademarks in names. Protocol identifiers that games expect stay in the code where they are technically needed.
6. No stand-in for a publisher service that still runs, and no bypassing of anti-cheat or copy protection. A game whose online play runs on such a service stays offline on the PlayStation side. Where the publisher has shut a service down, a revival server may bring the game's online play back. It is written from observation of the game the player owns, never from the publisher's code or keys, and it stops if the publisher brings the service back.
7. Only games the player owns. The cloud never sees game files; it knows a title ID and a game version so that only matching versions play together.

## Parts

### Bridge (in an AnyPS5 build)

The bridge is small on purpose. It forwards; it does not translate.

- **Switched on by the launcher.** The launcher sets `ANYPS5_NET_ENDPOINT` (a loopback port) and `ANYPS5_NET_TOKEN` (a secret for one game start). Without them every library behaves as it does today: offline and signed out.
- **One connection** to the net service, speaking the [bridge protocol](../spec/bridge-protocol.md). Request bodies pass through as opaque bytes, so the engine needs no JSON, WebSocket or ICE code.
- **What it forwards:**
  - sign-in state and its changes, reachability, the handle and account ID;
  - the online-permission check. The target is the network's own rule: signed in and not banned, for play on the AnyPlay network only ([ADR 0021](adr/0021-the-line-is-drawn-at-the-law.md)). The bridge answers it by this rule;
  - NAT information measured by the net service;
  - every web API request (user context, API group, path, method, headers, body) and its response, plus push events in the other direction;
  - session signaling: activation, connection state and connection info;
  - the join intent for a starting game, and the result of the invitation dialog.
- **Game traffic.** Peer-to-peer sockets are accepted, and each player in a session gets a virtual address from `198.18.0.0/15`, one that no other member of that session has ([ADR 0020](adr/0020-virtual-addresses-belong-to-sessions.md)). The range is reserved for testing and never routed on the internet. Whatever a game sends to a virtual address goes over a loopback socket to the net service and through the tunnel. The receiving game sees the sender's virtual address. Reliable peer-to-peer streams become QUIC streams in the tunnel. The two loopback hops add an estimated 0.1 ms or less.

### Net service (this repository)

A separate process that the launcher starts and watches ([ADR 0004](adr/0004-net-service-runs-as-its-own-process.md)), `anyplay-net` in `crates/net`. The launcher talks to it over its standard streams ([ADR 0006](adr/0006-the-launcher-talks-to-the-net-service-over-its-standard-streams.md), [control protocol](../spec/control-protocol.md)). One task owns its state and handles commands, cloud events, bridge frames and tunnel changes one at a time; requests to the cloud run beside it and report back. Game packets never pass that task: they go from the bridge's loopback socket straight into the tunnel.

- **Account and device.** The refresh token is kept in the operating system's credential store, with the key of the device that it is bound to. The player signs in and manages the account on the cloud's web pages in their browser ([ADR 0016](adr/0016-accounts-are-managed-on-the-web.md)).
- **A lasting connection to the cloud** for presence, invitations, friend requests and session events.
- **Translation.** It turns the platform-shaped web API requests from the bridge into the cloud's own API, with one handler per API group. `userProfile` and `communicationRestrictionStatus` are answered today ([spec/web-api](../spec/web-api/README.md)); sessions, matchmaking and the profanity filter come next, leaderboards and title storage later. Anything unknown is refused and written to the observation log ([ADR 0001](adr/0001-bridge-forwards-net-service-translates.md)).
- **Tunnels** on [iroh](https://www.iroh.computer/) 1.0: QUIC, hole punching, relays as the fallback, encrypted end to end ([ADR 0002](adr/0002-tunnels-on-iroh.md)). Each time a tunnel settles on a direct or a relayed path, the service logs one line on standard error with target `anyplay::path`, the kind of path and running totals (`direct`, `relay`, `upgrades`). The line names no one and nothing leaves the PC: it measures our own test machines and probes. Measuring real players needs a report to the cloud, which the specification does not have yet.
- **Party voice** ([`voice.rs`](../crates/net/src/voice.rs), [spec](../spec/voice.md), [ADR 0010](adr/0010-voice-and-shared-play-go-through-the-tunnels.md), [ADR 0014](adr/0014-voice-is-an-engine-without-a-transport.md), [ADR 0017](adr/0017-voice-has-a-connection-of-its-own.md)). When the player has switched voice on and is in a party, one task, the **hub**, owns the voice engine and with it the microphone and speakers; with voice off or no party nothing is opened. For each other member who takes part it holds a second QUIC connection on the tunnels' own iroh endpoint, with the ALPN `anyplay/voice/0`, because game datagrams carry no tag byte to tell them from voice. It is a connection of its own but not a path of its own: relay-only clears the endpoint's direct addresses, so voice goes through the relay whenever the tunnels do, and a restart of the tunnels (relay-only changed, new tickets) hangs voice up and joins it again on the new endpoint. The hub pumps the engine every 5 ms, sends each frame it produces as a datagram to every member, and gives each member's datagrams to the engine under that member's account.
  - **Who may connect** is the party's member list and nothing else. The cloud's `Party` carries each member's tunnel key, relay and a `voice` flag, which each net service announces with `PUT /v1/party/endpoint` ([cloud API](../spec/cloud-api.md#parties)); sessions do the same with `Member`. A connection stays only if the key the handshake proves is a current member's key; anything else is closed at once and never reaches the engine. A key that two members name belongs to neither. As for tunnels, the smaller key of a pair dials and the larger accepts.
  - **Membership follows the party's revision.** Every accepted copy of the party goes to the hub, which closes connections to members who left or changed their key, forgets them in the engine, and dials new ones; a copy with an older revision never gets there. A dial that fails is repeated with a pause of 0.5 s growing to 30 s.
  - **Sound devices** sit behind the `devices` feature of `anyplay-net` (cpal). Release builds on Windows and Linux are built with `--features devices`; on Linux that needs the ALSA headers (`libasound2-dev`). Without the feature, which is what `cargo build` and the usual CI jobs use, the service builds and runs and voice answers that this build has no sound devices. A CI job builds and lints the feature on both systems.
  - The launcher gets a `voice` event with the state, who is speaking and each link, at most ten a second, and sets push-to-talk, mode, volume, mute and the devices with the commands in the [control protocol](../spec/control-protocol.md). The launcher never sees tunnel keys.
- **Share screen** ([spec](../spec/screen-share.md), [ADR 0023](adr/0023-share-screen-is-watch-only-and-asks-the-cloud-for-nothing.md)). The watch-only part of share play: a player streams their game to the party, whose members choose to watch. A connection with the ALPN `anyplay/screen/0`, dialed by the streamer to each party member, carries share play's datagrams; no cloud state, no `share.enabled`. Built, with game sound on Windows; the share worker also publishes the picture live over RTMP for broadcasts.
- **Share play** ([spec](../spec/share-play.md), [ADR 0019](adr/0019-share-play-streams-through-a-worker.md); designed; built so far: the part that share screen uses, the bridge's `0x05xx` kinds with the frame ring and the guest pad on both sides of the bridge, and the host's pad rules. Not built: the invitation, the cloud's `Share` and the share connection). A third connection on the tunnels' endpoint, with the ALPN `anyplay/share/0`, carries a game's picture and sound to one party member and that member's pad states back. A separate share worker encodes and decodes; the net service checks who connects, validates and rate-limits pad states, and forwards datagrams.
- **Version guard** before every join.

### Cloud ([anyplaynet/cloud](https://github.com/anyplaynet/cloud))

- Accounts, devices and bans.
- Friends, blocks and presence.
- Sessions: a general object with title, version, members, leader, capacity, joinability and opaque properties.
- Matchmaking tickets per title and mode; a plain queue first, region and latency later.
- Invitations, and signaling that exchanges the tunnel keys of the players in a session.
- Relays (`iroh-relay`), self-hosted, with bearer-token access so that only network accounts can use them.
- Profiles, trophy records, a feed, messages and parties; voice and share play go through the tunnels ([social design](SOCIAL.md)).
- Later: leaderboards, save backups.
- Moderation from the first public day: reports, blocks, rate limits, bans per account and device.
- One `docker compose up` starts a complete cloud, and the launcher can point at any cloud. Nobody depends on a single operator.

## A join, step by step

1. A is playing. The game creates a player session; bridge, net service and cloud record it. A's friends see "playing X · joinable".
2. B selects **Join** in the launcher, or accepts an invitation. The version guard checks both sides, the launcher starts the game, and the net service holds a join intent with the session ID.
3. The game picks up the intent and joins the player session. The cloud adds B and sends A a push event.
4. Both games activate session signaling. The net services exchange keys through the cloud and open a tunnel, direct or through a relay. Each game gets "connected" and the other player's virtual address.
5. From here, game traffic flows through the tunnel.

A link such as `anyps5://v1/join/…` never starts a game by itself; it opens the launcher on the **Join** button, and the player presses it. The link on a player's Discord profile, `https://<web>/join/<code>`, is the same kind of link: it carries a [join code](../spec/cloud-api.md#join-codes), shows who invites and to what on the web, and leads to the launcher, where the code becomes an ordinary invitation ([ADR 0025](adr/0025-a-join-link-asks-it-never-joins.md)).

## What a game sees

| The game asks | The network answers |
|---|---|
| Am I signed in? | Yes while the net service holds a valid session; changes arrive as callbacks. |
| Who am I? | The handle as online ID (at most 16 characters, the size games reserve) and a 64-bit account ID from the cloud. |
| May I use online features? | Yes while the player is signed in to the network and not banned. The answer speaks for the AnyPlay network only, never for a platform subscription ([ADR 0021](adr/0021-the-line-is-drawn-at-the-law.md)). The bridge answers by this rule (anyplaynet/AnyPS5#24), and the answer needs a lawyer's review before the public launch. |
| Which NAT type? | Measured by the net service from iroh's net report: 1 when nothing stands between the PC and the internet, otherwise 2 while a relay answers, since every peer reaches it through the relay. |
| Authorization code or ID token for client X | Only for the network's own clients; an error for any publisher's client. |
| Create, join or leave a session; set its properties | A cloud session. |
| Submit a matchmaking ticket | The cloud queue; the result arrives as a push event. |
| Friends, presence, profiles, block list | From the cloud. |
| Connect to another player | A tunnel with a virtual address. |
| Invite friends | A friend picker in the launcher; the invitation goes through the cloud. |

## Limits

- Publisher services that still run stay out, and with them every game whose online play needs one. Revival servers for services a publisher has shut down are allowed ([ADR 0021](adr/0021-the-line-is-drawn-at-the-law.md)) and none is built yet.
- There are no dedicated servers. What works is peer-to-peer play or one player hosting.
- Session and matchmaking formats are not publicly documented. They are learned game by game from observation.
- Many games run slowly on AnyPS5 today. Network code with timeouts can give up at low frame rates, so the network grows alongside AnyPS5's performance work.

## Roadmap

| Phase | Content | Done when |
|---|---|---|
| 0 Probes | iroh tunnel between two real networks; bridge in observation mode | direct and relayed paths are measured; one game start has an observation log |
| 1 Network without games | cloud v0 (accounts, friends, presence), net service, online area in the launcher, self-host compose file | two players see each other in the launcher |
| 2 Bridge | sign-in, identity, NAT, web API forwarding, push events, tests with a fake game | a game signs in and shows its handle |
| 3 Tunnel | peer-to-peer sockets, virtual addresses, session signaling | two fake games exchange packets over the internet, direct and relayed, in CI |
| 4 Sessions | sessions, invitations, join intents, invitation dialog, version guard | two players play a class A game together over the internet |
| 5 More | matchmaking, leaderboards, save backups | by choice |
| S1–S6 Social | profiles and trophies, parties and messages, voice, feed and boards, share play, web profiles and sign-in ([SOCIAL.md](SOCIAL.md)) | each phase as SOCIAL.md says |

Where it stands (26 October 2026):

- **Phase 1 is done and live.** The cloud runs at anyplaynet.com as one node, and the launcher's online area uses it: accounts, friends, presence, messages, parties, voice and share screen. The social phases S1 to S6 are nearly done: share screen, the watch-only part of S5, is built, and handing a pad to a friend is open.
- **Phase 2 is nearly done.** In the AnyPS5 fork, sign-in, identity, NAT type, web API forwarding, push events, the online-permission check and the frame tests are merged (anyplaynet/AnyPS5#7, #10, #24). The end-to-end CI that runs the bridge against the net service is in review (#9). The net service measures the NAT type from iroh's net report and answers `userProfile` and `communicationRestrictionStatus`.
- **Phase 3 is mostly done.** The fork carries peer-to-peer datagrams, session signaling and streams (#11, #12); fake games exchange packets through tunnels in CI.
- **Phase 4 is open.** The first game played together over the internet is not there yet. The current target is a game that reaches its title screen.
- **Phase 0 waits.** The probe is ready; a measurement between two real networks needs a second network.

What each part still lacks is in [docs/gaps.md](gaps.md).

Until AnyPS5 decides otherwise, the bridge ships in an AnyPlay build of AnyPS5 that the launcher offers as a toolkit channel. Players who don't pick it stay offline as before. For AnyPS5 itself we will propose only the neutral part: one variable and an openly documented protocol between its network libraries and an outside service, with today's offline behaviour when the variable is unset.
