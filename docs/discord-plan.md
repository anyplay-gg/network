# Discord Social SDK: plan

The order of work for the choices in [ADR 0022](adr/0022-discord-social-sdk.md). Status: proposed. Sizes (S, M, L) order the work and promise nothing: S is days, M a couple of weeks, L a month or more for one person.

Rules for every phase:

- CI never talks to Discord and never has the SDK: tests run against the fake `Social` implementation. A change that touches the SDK is checked by hand with two Discord accounts (an invite cannot be received by the account that sent it).
- Nothing Discord-related is stored in the cloud (no token, no friend, no ID list). A test asserts it.
- Discord features are off until the player connects; presence on Discord is a toggle that starts on after connecting.
- The SDK binary, its header and its notices never enter a repository.

## What the SDK 1.10.19337 contains

Inspected from the archive, nothing run: a C API (`include/cdiscord.h`, about 350 `Client` functions) and a C++ wrapper that only forwards to it (`include/discordpp.h`); `discord_partner_sdk.dll` for Windows x64 (9.9 MB) and ARM64, `libdiscord_partner_sdk.so` for Linux x86-64 only, `libdiscord_partner_sdk.dylib` (universal) and frameworks for macOS, `.aar` and `.xcframework` for Android and iOS, debug builds (up to 200 MB each), Krisp noise-cancellation libraries and models (15 MB and up), and `License-Notices.txt` (Boost and MIT notices for function2, jsoncpp and others). No licence text of Discord's own ships in it; the terms are the web page "Discord Social SDK Terms" and the Developer Terms and Policy. The archive is 781 MB, nearly all debug and mobile; what we would ship is under 30 MB per platform without Krisp.

## Phases

| Phase | What the player gets | Work | Size |
|---|---|---|---|
| 0 Spike | Nothing visible. The SDK loads on Windows and Linux, the code exchange runs through a cloud route, `Connect` succeeds, the current user and the number of friends are read. Decides whether in-process is stable. | `crates/discord` with the trait and `Off`, the loader, the cloud routes `POST /me/discord/token` and `POST /me/discord/refresh` (exchange and refresh, nothing stored, fresh session when a connection is created), a debug command in the net service. | S |
| 1 Presence and invites | "Playing Title" on the Discord profile without the Discord app running, party size, "Ask to join" and invites that open the launcher on the party. Replaces the IPC presence while connected. | Control protocol: `discord_connect`, `discord_disconnect`, `discord_presence {visible}` and the events `discord_state`, `discord_join`. Activity from the party and the running title (`Activity::SetName` for the title, external image URLs for art); join secret is an opaque party ticket from the cloud, redeemed with the party's own rules (blocks, privacy, size). `RegisterLaunchCommand` for the launcher's URL scheme so an invite opens a closed game launcher. Launcher: consent screen, Connect and Disconnect, the toggle, the settings entry "Discord Social Experience" (`OpenConnectedGamesSettingsInDiscord`). Token store in the credential store. | M |
| 2 Unified friends list | Discord friends beside AnyPlay friends: "Online" (in AnyPlay), "Online elsewhere" with the Discord badge, "Offline"; invite a Discord friend to the party; add, remove, block and ignore from the list. | `discord_friends` events from `GetRelationshipsByGroup` and its update callback; local merge with the Discord IDs AnyPlay friends have made visible; `discord_invite {friend}` through `SendActivityInvite` (no message text, so no communication scope); friend requests and blocks. Launcher: the list, the connection point in the friends list until connected, Discord's strings. | M |
| 3a Voice setting | The setting "Voice: AnyPlay / Discord". In Discord mode the net service does not open party voice and the party shows "Open Discord", which opens the player's Discord app (a `discord://` link). Not an integration; honest about it. | A flag in the party's voice start, an "Open Discord" action. | S |
| 3b Voice over a Discord lobby | For players who all connected: the party's voice runs through a Discord lobby (`CreateOrJoinLobby`, `StartCall`) instead of AnyPlay's QUIC voice. Mute, deafen and volume stay AnyPlay's controls. Players who did not connect stay on AnyPlay voice, so a party can be split. | Scope `sdk.social_layer`; a second voice engine behind [ADR 0014](adr/0014-voice-is-an-engine-without-a-transport.md)'s seam; Krisp decision. **Waits for Discord's approval** (below) and for evidence that players want it, since AnyPlay's own voice already works for everybody. | L |
| 4 Messages | None for now. A Discord friend's row opens the conversation in Discord. Linked channels (lobby to server text channel) only with groups, if ever. | No work. Revisit after phase 3b. | none |

Phase 1 can ship on its own. Phase 2 is what Discord's review looks at first. Phases 0 to 2 touch `crates/discord`, `crates/control`, `crates/net` and `spec/control-protocol.md` here, the portal routes in the cloud, and the launcher's online area.

## Accounts and tokens

1. The player presses Connect in the launcher. A screen of ours says what happens: friends and their status are shown here and not stored by AnyPlay, the title and party size show on the Discord profile, it can be turned off any time.
2. The net service calls `Authorize` with the scopes and a PKCE challenge; the overlay, the Discord app or the browser asks the player.
3. The net service posts the code, verifier and redirect to the cloud; the cloud exchanges it with the secret and returns the tokens; the identity is checked against the connection.
4. The refresh token goes into the credential store; `UpdateToken`, then `Connect`. A callback shortly before expiry refreshes through the cloud.
5. On `invalid_grant` the stored token is dropped and the launcher shows "Discord needs to be connected again". Disconnect revokes the token at Discord, deletes it, clears all Discord data from memory and sends an empty activity.

The Discord application needs: Public Client off, the redirect `http://127.0.0.1/callback` beside the portal's callback, a description, privacy policy and terms links, the invite image and art assets.

## Licensing and distribution

What the terms say (Social SDK Terms of 17 March 2025, with the Developer Terms and Policy):

- The licence is limited, non-exclusive, non-transferable and revocable, and covers "to distribute the Discord Social SDK as integrated into your Application". Copying, reproducing, redistributing or syndicating it otherwise is forbidden (2.b), as is reverse engineering. So shipping the library inside the launcher's release is covered; a copy in a repository, a separate download offered on its own, or a "Discord SDK pack" is not clearly covered.
- Discord may change or require another version of the SDK, end support, and revoke access at any time, then we delete what we hold (4, 6).
- Discord reviews the integration against its requirements before launch (3.a). Age: no one under 13 (3.b).
- For people who did not link a Discord account to ours, personal data may be processed only to run the integration (5.a.ii). Developer Policy: use API data only for the stated function, no profiling of users or their relationships, no disclosure to data brokers or ad services, no sale, no training on message content (15 to 21).

What follows for us:

- **Repositories:** nothing of the SDK, not even a copy "for convenience". The public repositories take only MIT, Apache or OFL code, so the SDK is a runtime file, not a dependency. The bindings are made by `bindgen` from the header during the release build and land in the build directory.
- **Where it is kept:** a private repository of the organisation used only for this (not `hq`, which is for reports), holding one GitHub release asset: a trimmed archive of the shipped platforms' libraries and `License-Notices.txt`, never the debug or mobile files. This is internal storage for our own build, which we read as part of developing the integration; the question to Discord below covers it.
- **How the build gets it:** the launcher's release workflow (which already builds the net service from a pinned commit) downloads the asset with a fine-grained read-only token held as a secret of a protected `release` environment, compares its SHA-256 with the digest written in the public workflow, unpacks the libraries next to the net service, and adds the notices to the release's third-party file. Pull requests, forks and the network repository's CI have no secret and build with the fake. The release asset page then shows the digest and the SDK version in `BUILD-INFO.txt`.
- **Loading:** the net service loads the library by absolute path from its own directory only (no search path, no current directory) and refuses to start Discord features if it is not there.
- **Updating:** a pull request changes the digest after the operator has uploaded a new archive and the two-account check passed.

## Privacy and the line

- [ADR 0021](adr/0021-the-line-is-drawn-at-the-law.md): the SDK talks only to Discord and carries only Discord's own tokens for the player's own account. It touches nothing of a console platform or a publisher. The activity shows the title the player runs, as the IPC presence does today; the Discord application's name and art carry no platform trademark.
- [ADR 0012](adr/0012-private-by-default.md): nothing is on until the player connects; a connected player chooses whether Discord sees the activity; private parties show nothing. Discord friends never reach the cloud. The privacy policy names Discord as a recipient of the activity and as an independent controller of its own data, and describes disconnecting.
- What may be held, and where: refresh token in the credential store; friends, status and avatars in memory only; nothing in logs (the SDK's log callback is mapped to our log with Discord IDs and tokens removed); nothing in crash reports.
- Interface rules from Discord's guidelines, for the launcher: the Discord logo unaltered and wordmarks from discord.com/branding; sign-in and friends-list buttons in Discord's styles; the Discord display name for people we know only from Discord; the AnyPlay name first for a friend who has both; the Discord badge only on friends online elsewhere with a linked account; "Discord Social Experience" for the settings entry; a success and an error state after connecting, in our own style; a persistent call-to-action in the friends list until the player has connected.

## What the operator has to do

Free of charge: the SDK, phases 0 to 2, and the application. Nothing here costs money.

1. In the Developer Portal for application 1563714917998133288: Public Client off; add the redirect `http://127.0.0.1/callback` (the portal callback stays); name, description, icon, privacy policy and terms URLs; upload the Rich Presence assets and the invite image; add the second test account to the team. The client ID and secret go into the cloud's settings (`discord_client_id`, `discord_client_secret`, already used by the portal link).
2. Create the private repository and the release asset with the trimmed archive; give the release environment its read-only token. Only the operator's login can download the archive from Discord, so only the operator can refresh it.
3. Before the public launch: ask a lawyer, and if possible Discord's developer support, whether (a) our public release assets carrying the library count as "integrated into your Application", (b) the private copy is fine, and (c) the age of consent for connecting. Tell Discord about the launcher before launch, since Discord may test the integration (3.a).
4. Only for phase 3b or any text: the Comms Access form in the Developer Portal (Social SDK, Comms Access), with the feature set live and an unlisted video of it; Discord may refuse. Not needed before.
5. Download Discord's branding assets once and hand them to the design pass.

## Uncertain

- Whether a launcher counts as a game for Discord's review and how it treats a presence that names other titles. Phase 1's presence works under the application either way; the review decides about phase 3b.
- Whether declaring the C API's functions and structures in our own build step is a "derivative" in Discord's sense. We use `bindgen` on a private copy and commit nothing, which is the cautious reading; Discord could say otherwise.
- Whether a refresh token replaced at each use survives a crash between Discord's answer and our save; if not, the player connects again.
- Whether `identify` can be asked in the same flow as the SDK's scopes (the documentation allows more scopes). If not, identity comes from the token's `openid` claims.
- How stable the library is inside a long-running process on Linux (marked experimental) and whether the overlay-less browser flow is good enough there. Phase 0 answers it.
- What a closed or crashed Discord app does to the SDK's own connection: the documentation says presence works without the app, which phase 0 checks.
