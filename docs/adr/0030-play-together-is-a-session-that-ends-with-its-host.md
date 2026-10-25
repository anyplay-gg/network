# Play Together is a session that ends with its host

A game that finds others on a local network (Minecraft's LAN worlds) has no session calls of its own, so nothing in it opens a cloud session. **Play Together** gives it one: when the host's launcher starts such a game with friends in mind, the net service opens a session of a new `kind`, `lan`, before the game starts. Everyone who joins that session is on the same virtual LAN ([ADR 0021](0021-the-line-is-drawn-at-the-law.md)): the net service already copies a game's broadcasts to the members of "the session the launcher shows" ([bridge protocol](../../spec/bridge-protocol.md#broadcasts-and-the-virtual-lan)), so this session is simply the one it finds. Nothing is added to the bridge.

A `lan` session is the ordinary cloud session with three differences:

- **Who opens it.** The host's net service, on `game_start` with a `host` plan, not the game. A launcher asks for one when the player presses Play Together; a plain Play opens none.
- **It ends with its host.** The leader of a `lan` session is the world host: the world lives in the host's game. When the leader leaves, is removed or stays disconnected past the grace period, the cloud closes the session and tells the others with `session_gone`, where it hands the lead on in a `game` session. The leader cannot be changed. A member who leaves just leaves.
- **Everyone sees it for what it is.** `Session`, `Invitation` and presence's `SessionRef` carry `kind`, so a launcher says "Mira is playing Minecraft. Join?" and "Playing Minecraft together (2/4)" and does not call it an invitation to a game session.

Joining is the join that exists: an invitation, a party move, a friend's presence, a join code. The version guard is the one that exists too: `title_id`, `version` and the AnyPS5 `build` must match, because a LAN game refuses a peer on another version and a toolkit that differs can break the wire. What is new is only that the launcher compares them before it starts anything, from the session and the player's own library, and names exactly what is missing.

## Considered options

- **A flag in `properties`.** No schema change, but `properties` is the game's opaque data, the cloud could not enforce the host rule on it, and presence could not tell a LAN session without reading every session's bytes.
- **A room object next to sessions** (`/v1/rooms`). Cleaner words, but a second copy of membership, visibility, invitations, blocks, addresses, the grace period and the events, for a thing that is a session in every way that matters. Tunnels, virtual addresses and broadcasts already follow sessions.
- **The party is the session.** A party stays together across games and has no addresses ([ADR 0020](0020-virtual-addresses-belong-to-sessions.md)); a friend who is not in the party could not join, and the LAN would end when someone left the party.
- **The bridge activates the session.** A LAN game calls no session function, so the bridge would have to guess that "this game wants a LAN". The launcher knows which titles are LAN games and what the player asked for, and the net service owns the cloud calls.
- **Hand the lead on when the host leaves.** For a game session that keeps the others together. For a LAN world it would leave them in a session whose world is gone and whose host nobody can hear.

## Consequences

- A cloud that predates the field reads `kind` as `game`; a net service that predates it never sets it. Old and new meet without trouble: the worst case is a `lan` session that a very old launcher shows as an ordinary session.
- The host presses Play Together for a title in the launcher's LAN table; whether a game is a LAN game is the launcher's knowledge, and the public compatibility list can reuse the same table later.
- An account is in at most 4 sessions, so hosting counts against it as any session does.
- A member's own game going on after the host left is the game's business: the launcher says the session ended and leaves the game running.
