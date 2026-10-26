# Play Together

How friends play a LAN game together without setting anything up. A player starts the game from the launcher with Play Together; a friend presses Join; the friend's launcher starts the game in the same session; the host's world shows up for the friend as a LAN world. Nobody opens a port, sets up a VPN or types an address.

The decision and its alternatives: [ADR 0030](../docs/adr/0030-play-together-is-a-session-that-ends-with-its-host.md). Version 0, a draft.

## The parts

| Part | What it does |
|---|---|
| **Launcher** | Knows which titles are LAN games (its table, below), whether the player owns a title and with which version and toolkit, and what the player asked for. Starts and stops games. |
| **Net service** | Opens the session for a hosted game, joins one for a joined game, asks the party in, tells the launcher when the session ends. Carries the LAN traffic ([bridge protocol](bridge-protocol.md#broadcasts-and-the-virtual-lan)). |
| **Cloud** | Keeps the `lan` session, its members, addresses and invitations ([cloud API](cloud-api.md#play-together-sessions)), and closes it when the host leaves. |
| **Bridge** | Nothing new. It presents the virtual LAN to the game and hands broadcasts to the net service. |

## Hosting

1. The player presses Play Together on the game's page, or in the party. The launcher sends `game_start` with `host` (`max_members` from its LAN table, `visibility` `friends`, `party` true when the player is in a party).
2. The net service asks the cloud for a session of kind `lan` with the game's title ID, version and toolkit build, and the player's tunnel endpoint. The cloud makes the player its leader and gives it a virtual address. The net service answers `game_start` with the session's ID and the game starts with the bridge on.
3. The party, if any: the net service moves it into the session when the player leads the party, else invites each other party member. Each friend gets an `invitation` of kind `lan`.
4. Friends who are not in the party see the player's presence: the title and a `SessionRef` of kind `lan` that is joinable while the session has a free place. The launcher words it "playing together".
5. The game opens a world to the LAN. It broadcasts to the virtual subnet and the net service carries it to the other members, whose own games answer.

The session is the game's only session: it is the newest, which makes it "the session the launcher shows", the audience of broadcasts.

When the cloud, the network or the player's online features are off, the game starts as any game does. If the net service cannot open the session, it says `host_failed` and the launcher tells the player why; nothing blocks the game.

## Joining

A friend can join from an invitation (party members get one), from the friend's presence in the Friends list or profile, or from a join code. All of them end in the same two steps.

**1. The check.** The launcher compares, in this order, the offer with what it knows, and stops at the first that does not hold. The answer names exactly what is missing. The offer is the invitation or the friend's presence (title ID, title, version, kind, the host's handle, places left); the library and the toolkit are the joiner's own.

| Check | Fails when | The player is told |
|---|---|---|
| Signed in and online | the launcher is signed out or the cloud cannot be reached | to sign in, or that they are offline |
| Online play on | the player switched the online features off | to switch them on |
| Owned | no game in the library has the title ID | which game they need |
| Version | the library's version of the game does not fit the offer's under the title's version rule (below) | "You need *Minecraft 01.044.000*; you have *01.008.000*" |
| Toolkit | the session's AnyPS5 build differs from the one the game would start with | the build they need and have |
| Place | the session is full | that it is full |
| Not busy | another game is running | to close it first |
| Still there | the cloud no longer knows the session | that it ended |

The first four are local. The toolkit, place and presence are the net service's `check_join`, which asks the cloud for the session and answers `version_mismatch` (with `expected`), `full` or `not_found`. An unlocked copy is not required to press Join: if the game is owned but not yet unlocked, the launcher unlocks it and then starts it in the session.

When the version check fails, the card also offers **Try Anyway**, a quiet second action beside the primary one. It says in the card's words that the game may not connect or may fall out of sync, and it joins regardless: the launcher sends the same `check_join` and `game_start` with `override_version` true, which skips the version part of the net service's and the cloud's guard and nothing else (a different AnyPS5 build still blocks), and writes the override to its log.

**2. The start.** The launcher sends `game_start` with `join` set to the session. The net service joins, keeps the join intent for the game, and the bridge answers the game's LAN calls as for the host. The joiner's game finds the host's world through discovery on the virtual LAN. The game, not the launcher, opens the world.

A friend whose presence shows the title but no joinable session is playing without Play Together, or has the online features off: the launcher offers nothing to join and says so.

## Ending

- The **host's game ends** (or the host's net service stays gone past the grace period): the cloud closes the session, and each other net service sends `session_ended`. The launcher says "*Mira*'s session ended" and leaves the game running; the world is gone with its host.
- A **member's game ends**: that member leaves, the others see one member fewer and the session goes on.
- The host **removes** a member: the removed member's net service learns it as the session ending for them.

## Presence

The launcher shows a friend in a `lan` session with "Playing *Minecraft* together (2/4)": the title, the word, and the places taken of the places there are. For the player's own session it is the same line. A session of any other kind keeps today's wording.

## The LAN title table

The launcher holds a small curated table, `data/lan-titles.v1.json`, of the titles that can be played together this way. A copy is built into every launcher. The table is the launcher's and the public compatibility list may reuse it later; the cloud has no copy and decides nothing from it.

```json
{
  "schema": 1,
  "updated": "2026-10-25",
  "titles": {
    "PPSA17221": { "name": "Minecraft", "mode": "lan", "max_players": 8, "version": "exact" }
  }
}
```

- `mode` is the online mode: `lan` (the game finds others by local network broadcast). Other modes will be added when a title needs one; a launcher ignores an entry whose mode it does not know.
- `max_players` is the most players the game allows in one world, host included; it is the `max_members` of the session.
- `version` is how far the host's and the joiner's versions of the game may differ: `exact` (the same version; also what an entry without the field means), `major` (the same first dot-separated part: `01.044.000` fits `01.050.000`, not `02.000.000`) or `any`. A game that refuses another protocol version stays `exact`; `major` and `any` are for games seen to play across patches. A value this launcher does not know refuses the file, like any other nonsense. The field is optional, so the schema stays 1 and launchers from before it read the file and treat every entry as `exact`.
- **Where the rule is applied.** The table is the launcher's, so the launcher reads the rule and sends it with the join (`version_rule` on `check_join`, `game_start` and the cloud's join). The net service and the cloud apply it by comparing two version texts and know no title. Nothing is stored in the session, so changing a rule in the table needs no migration.
- A title is in the table only after someone has seen it work (an observation log, or a run on two PCs), with the evidence in the pull request.
