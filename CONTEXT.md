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

**Device**:
One PC that is signed in to an account, with its own tokens. An account has at most 10.
_Avoid_: Session (that is a group of players), client

**Device key**:
The key a device makes when it signs in. Its refresh token is useless without it.
_Avoid_: Certificate, API key

**Passkey**:
A credential of the player's phone, computer or security key that signs them in on the account web, without a code.
_Avoid_: Biometric login, WebAuthn (the standard behind it)

**Account web**:
The server-rendered pages of the cloud, in the player's browser, where they sign in, create an account and manage it. The launcher only starts the browser flow. The portal replaces them.
_Avoid_: Dashboard

**Portal**:
The web app the cloud serves from its own origin, where a player signs in, manages the account, and sees friends, trophies and profiles. It talks to the cloud through the portal API.
_Avoid_: Dashboard, site, web client

**Browser session**:
A browser's own session on the cloud, with its cookie and CSRF token, apart from the tokens of devices. An account can have several, and ends each one by itself.
_Avoid_: Login, device (a device is a PC with tokens)

**Step-up**:
Proving again, with an e-mail code or a passkey, who the player is before an action that would give a thief of the session a lasting hold. A session that signed in or stepped up in the last 15 minutes is fresh.
_Avoid_: Re-login, reauthentication

**Grace period**:
The 30 days between the player asking to delete the account and the cloud deleting it. They can cancel until it ends.
_Avoid_: Cooling-off, trash

**Sign out everywhere**:
Ending every device and every browser session of an account at once, and refusing every access token issued before.

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

### Social

**Profile**:
What a player shows of themselves: handle, avatar, a short text, favourite games and playtime, each part visible to friends, everyone or nobody.
_Avoid_: Account (the sign-in identity behind it)

**Avatar**:
The picture a player chose for their profile: a square the cloud made from the player's own PNG or JPEG file. Never taken from a game.
_Avoid_: Icon, profile picture (when the cloud's square is meant), game art

**Playtime**:
The minutes a player spent in a title, counted per title and per day by the launcher. The cloud keeps the highest number it has seen for a day, so a repeated upload does not count twice.

**Gamer card**:
The short form of a profile that friends see in their list: avatar, handle, what they play, trophy count. It holds nothing else, so it stays small.

**Trophy record**:
The network's note that a player earned one trophy: title ID, the trophy's number in the game, its grade and the time. Names and pictures of trophies are not part of it; they come from each player's own copy of the game.
_Avoid_: Achievement (the word of other platforms), trophy (alone, when the record is meant)

**Rarity**:
The share of players with a title who earned a trophy, counted from trophy records.

**Feed**:
Recent activity of a player's friends: trophies, first starts of a game, sessions opened to friends.
_Avoid_: Timeline, wall

**Party**:
A group of up to 8 friends that stays together across games, with one chat and one voice channel. A party belongs to no game; a session does.
_Avoid_: Group, squad, session

**Party leader**:
The member of a party who invites, removes members and moves the party into a session. The longest-standing member leads when the leader leaves.
_Avoid_: Host, owner

**Voice connection**:
The encrypted connection between the net services of two party members that carries their voice. It runs on the same endpoint as their tunnel and takes the same direct or relayed path, but is a connection of its own. Only members the party names can hold one.
_Avoid_: Voice tunnel, call, voice channel (that is the party's one channel as the player sees it)

**Conversation**:
A chat between accounts: two of them (direct), up to 32 (group), or the members of a party (the party's chat). Everything in it is text.
_Avoid_: Thread, channel, room

**Group conversation**:
A conversation of up to 32 accounts that is no party. It has no game and no voice; a party does.
_Avoid_: Group (alone; it is not a party), group chat room

**Message**:
A text of up to 2,000 characters written in a conversation. The cloud keeps it for a limited time.
_Avoid_: Post, DM

**Message number**:
The place of a message in its conversation: 1 for the first, then up by one. A client that holds the number it read up to asks for what comes after it.
_Avoid_: Sequence ID, timestamp

**Read mark**:
The message number a member has read a conversation up to.
_Avoid_: Seen flag

**Message request**:
A message from someone who is not a friend, held until the receiver accepts the conversation.
_Avoid_: Spam folder

### Moderation

**Report**:
A player's note that another account broke a rule, saved with a snapshot of what was reported. It outlives both accounts until its retention ends.
_Avoid_: Ticket, complaint, flag

**Moderator**:
An account with the moderator role, granted by the operator. It reads the queue of reports and takes actions.
_Avoid_: Admin (the admin token is the operator's bootstrap, not a role of players), staff

**Operator**:
The person or group that runs a cloud. Authenticates with the admin token and grants the moderator role.
_Avoid_: Owner, host

**Action**:
What a moderator does to an account, with a rule and a reason the player can read: a warning, a mute or a ban.
_Avoid_: Sanction, punishment, strike

**Statement of reasons**:
What the player is told about an action against them: what, why, which rule, for how long, how to appeal. It names neither the reporter nor the moderator.
_Avoid_: Ban message, notice

**Appeal**:
A player's one request, per action, that a moderator look at it again. A moderator other than the one who acted decides it when there is another.
_Avoid_: Complaint, objection

**Audit log**:
The append-only record of every moderation action, with who did it, to whom, why and when.
_Avoid_: History, ban log

**Restriction**:
A limit on what an account may do at the moment: too new an account, or a mute.
_Avoid_: Penalty

**Share play**:
A player streams their running game to a friend, who watches and may take over a pad. The friend needs neither the game nor its files.
_Avoid_: Remote play (playing one's own console from afar), screen share
