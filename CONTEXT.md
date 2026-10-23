# AnyPlay Network

The online service that lets players of AnyPS5 games meet and play together over the internet. This file is the glossary; the design is in [docs/DESIGN.md](docs/DESIGN.md).

## Language

### Parts of the network

**Network**:
AnyPlay's online service as a whole: the cloud, the net service on every player's PC and the bridge in every game.
_Avoid_: PSN, online service, backend

**Cloud**:
The central services of the network, run by AnyPlay or by anyone who hosts their own.
_Avoid_: Server, master server, backend

**Net service**:
The program on a player's PC that holds the account, talks to the cloud and carries game traffic to other players. The launcher starts it.
_Avoid_: Daemon, client, agent

**Bridge**:
The part of an AnyPS5 build that hands a game's network library calls to the net service. It forwards; it does not translate.
_Avoid_: Shim, hook, plugin

**Tunnel**:
The encrypted connection between the net services of two players, either direct or through a relay.
_Avoid_: VPN, link, socket

**Relay**:
A cloud machine that carries a tunnel when the two players cannot reach each other directly.
_Avoid_: TURN server, proxy

### People

**Account**:
A player's identity on the network: a handle, a verified e-mail address and the devices signed in to it.
_Avoid_: Profile (the launcher's local display name), user

**Handle**:
The unique public name of an account, at most 16 characters. Games see it as the player's online ID.
_Avoid_: Username, gamertag

**Friend**:
Another account that both sides agreed to connect with.

**Presence**:
What friends can see about an account right now: online or not, which game, and whether its session can be joined.
_Avoid_: Status, activity

### Playing together

**Session**:
A group of players in one game who can join, leave and see each other. Games call it a player session.
_Avoid_: Lobby, room, party

**Invitation**:
A request from one player to a friend to join their session.
_Avoid_: Invite link (that is a launcher deep link)

**Join intent**:
The instruction the net service holds for a game that is starting, naming the session it should enter.
_Avoid_: Launch argument, deep link

**Version guard**:
The check that two players run the same game version and a compatible AnyPS5 build before one joins the other.

**Virtual address**:
The address a game sees for another player in its session. It leads into the tunnel and exists only on the players' PCs.
_Avoid_: Peer IP, fake IP

### Games

**Title class**:
How much of a game's online part can work on the network. A: platform services only. B: platform services plus a publisher backend. C: publisher backend only. 0: no online part.
_Avoid_: Compatibility rating, badge

**Publisher backend**:
Servers that a game's publisher runs, such as its own accounts, matchmaking or game servers. The network never stands in for them.
_Avoid_: Third-party server, game server

**Observation log**:
The local record of every network library call a game made and what the bridge answered. The network's own specifications are written from it.
_Avoid_: Trace, capture, dump (a dump is a game folder)
