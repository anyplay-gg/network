# The line is drawn at the law

Supersedes [ADR 0003](0003-the-clean-line.md) and [ADR 0008](0008-the-online-permission-check-waits.md).

AnyPS5 should make every game the player owns playable together, and the network goes as far toward that as the law allows, from the start. ADR 0003 drew the line further in than the law does: it gave up every game whose online play ended with its publisher's servers, and ADR 0008 kept the online-permission check answering "not signed in", so games that ask before online play refused the network. Both cost games that no law asks us to give up.

The network does all of this:

- It answers every platform call a game makes for online play itself, with AnyPlay accounts: sign-in, presence, friends, sessions, matchmaking, signaling, peer-to-peer sockets, invitations, leaderboards, title storage and voice.
- It lets players play any game with local multiplayer together by streaming it, and hand the controller over in any game ([share play](../../spec/share-play.md)).
- It carries a game's local network traffic between players as a virtual LAN.
- It brings back the online play of games whose publisher has shut their servers down, with servers we write ourselves.
- It answers the online-permission check by its own rule: signed in to the network and not banned. The answer speaks for play on the AnyPlay network only, never for a platform subscription, and the network never presents itself as one.

Formats are learned only by observing and testing games the player owns, which Articles 5(3) and 6 of the EU Software Directive and §§69d and 69e UrhG allow, and are written down as our own specifications ([OBSERVING.md](../OBSERVING.md)).

The network never does any of this, because the law or other people's rights forbid it:

- contact Sony's servers, or use Sony's code, keys, firmware or leaked documents;
- run a game the player does not own, or help anyone get one;
- issue, forge or forward tokens for a platform's or a publisher's service, or present itself to one as a real console;
- stand in for a publisher's service that is still running;
- bypass anti-cheat or copy protection;
- carry a platform trademark in its names.

## Consequences

- Revival servers and the online-permission answer are built and tested now, while the network is open to the team only, and go through a lawyer's review before it opens to the public.
- A revival server is written from observation of the game the player owns: never from the publisher's code, never with the publisher's keys, and only once the publisher has ended the service. If a publisher brings a service back, our server for it stops.
- Games whose online play runs on a publisher's live service stay offline on the PlayStation side. Players who have the game's PC version play that one, with AnyPlay as the social layer around it.
- A public compatibility list says for each game how it plays online: through the network's platform services, as play together, as a virtual LAN, through a revival server, or not at all.

## Considered options

- **Keep ADR 0003 and ADR 0008:** the least exposure, but every game with shut-down servers and every game that checks online permission stays offline for no legal reason.
- **Also stand in for live publisher services:** the most games, but it replaces a service someone still runs and earns from. Courts have stopped exactly that (Davidson & Associates v. Jung, 2005; the injunction against a World of Warcraft private server in 2026), and it would end the project.
