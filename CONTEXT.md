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

**Handle skeleton**:
What a handle looks like once spellings that read the same in common fonts are folded together (`FIokzy` and `Flokzy`, `Flok_zy` and `Flokzy`). Two handles with one skeleton pass for each other, so only one account may hold it. Used for comparing; never shown.
_Avoid_: Normalized handle, handle key (the cloud's `handle_key` is only the lower-case spelling)

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

**Appearing offline**:
A setting of an account that makes its presence read as offline to every other account, while the player stays signed in and can still play, join, message and be invited. The player sees themselves as online, with a hint. It is not a block and not a way to go offline.
_Avoid_: Invisible, hidden, stealth mode, do not disturb

### Playing together

**Session**:
A group of players in one game who can join, leave and see each other. Games call it a player session. It is a game session when the game opens it and a LAN session when the launcher does for Play Together.
_Avoid_: Lobby, room, party

**Invitation**:
A request from one player to a friend to join their session.
_Avoid_: Invite link (that is a launcher deep link)

**Join code**:
A short random token the cloud issues to a player so that people outside their friends can ask to join. It stands for the player, opens the session they offer or else the party they lead, and expires or can be revoked. Redeeming it makes an ordinary invitation.
_Avoid_: Invite link (the link that carries a code is a join link), session code

**Join intent**:
The instruction the net service holds for a game that is starting, naming the session it should enter.
_Avoid_: Launch argument, deep link

**Version guard**:
The check that two players run the same game version and a compatible AnyPS5 build before one joins the other.

**Play Together**:
Starting a game that finds others on a local network so that friends can join it: the host's launcher opens a LAN session, a friend presses Join, and the host's world shows up for the friend as a LAN world. Nobody sets up a VPN, opens a port or types an address ([ADR 0030](docs/adr/0030-play-together-is-a-session-that-ends-with-its-host.md)).
_Avoid_: Multiplayer (alone), LAN party, hosting a server

**LAN session**:
A session of kind `lan`, which the launcher opens for a LAN game instead of the game itself. It ends when its world host leaves. Its members are on one virtual LAN.
_Avoid_: Room, lobby, LAN party

**World host**:
The leader of a LAN session: the player whose game holds the world the others join. Not the party leader, and not the share play host.
_Avoid_: Server, host (alone)

**LAN game**:
A title that finds other players by a broadcast on the local network and not through a session call. The launcher keeps a table of them.
_Avoid_: Local multiplayer (a game can have that without a network)

**Virtual address**:
The address a game sees for a player in a session. It belongs to the player's place in that session, is different for every member of it, and leads into the tunnel; it exists only on the players' PCs. Another session may give it to someone else.
_Avoid_: Peer IP, fake IP

**Virtual LAN**:
The members of one session seen as one local network by the game: their virtual addresses are in one subnet, and what a game sends to the broadcast address reaches all of them and nobody outside the session.
_Avoid_: VPN, overlay network

**Game broadcast**:
A datagram a game sends to a broadcast address to find others on its network. The net service copies it to the other members of the player's session.
_Avoid_: Multicast (not carried), discovery packet (that is one use of it)

### Games

**Title class**:
How much of a game's online part can work on the network. A: platform services only. B: platform services plus a publisher backend. C: publisher backend only. 0: no online part.
_Avoid_: Compatibility rating, badge

**Publisher backend**:
Servers that a game's publisher runs, such as its own accounts, matchmaking or game servers. The network never stands in for one that still runs.
_Avoid_: Third-party server, game server

**Revival server**:
A server the network writes for a game whose publisher has shut its online service down. It is written from observation of the game the player owns, never from the publisher's code or keys, and it stops if the publisher brings the service back ([ADR 0021](docs/adr/0021-the-line-is-drawn-at-the-law.md)).
_Avoid_: Private server, emulator

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

**Connection** (also **Linked account**):
The link between an AnyPlay account and an account of a provider such as Discord or Steam, made in the portal. It holds who the account is there (ID, display name, picture), never a token or anything the player has on the provider. At most one per provider, and an account of a provider belongs to one AnyPlay account. Each has its own visibility (friends, everyone or nobody). Making one needs a fresh session; removing one does not.
_Avoid_: Integration, social login (a connection is not a way to sign in)

**Discord session**:
The net service's signed-in state with Discord, made when a player connects Discord in the launcher: it shows the player's game and party on Discord and brings their Discord friends to the screen. It lives in the net service, holds a refresh token only in the credential store, and ends when the player disconnects. Not a connection, which is the portal's link.
_Avoid_: Discord login, Discord account link (that is the connection)

**Notification device**:
A browser that the player turned notifications on for in the portal, stored as a Web Push subscription with the kinds it wants (friend request, invitation, message). A notification names who it is from and never holds the text of a message. Per browser, off until the player turns it on, at most ten per account.
_Avoid_: Push token (a subscription is an address and keys of the browser's own push service)

**Gamer card**:
The short form of a profile that friends see in their list: avatar, handle, what they play, trophy count. It holds nothing else, so it stays small.

**Trophy record**:
The network's note that a player earned one trophy: title ID, the trophy's number in the game, its grade and the time. Names and pictures of trophies are not part of it; they come from each player's own copy of the game.
_Avoid_: Achievement (the word of other platforms), trophy (alone, when the record is meant)

**Trophy points**:
The worth of a trophy record by its grade (bronze 10, silver 25, gold 60, platinum 150). A player's trophy points are the sum over the records the viewer may see. Our own numbers ([ADR 0029](docs/adr/0029-trophy-points-and-levels.md)).
_Avoid_: Score, XP

**Trophy level**:
The level from 1 to 100 that a player's trophy points make, with the points at which it started and the next one starts. Shown on the profile next to the trophy counts and follows their visibility.
_Avoid_: Rank, account level

**Trophy comparison**:
Two players' trophy records in one title side by side (ID, grade and time each), with their totals, points and levels. It shows only what each player's trophy visibility allows; ids only, never names or art.
_Avoid_: Leaderboard (that ranks many players)

**Secret trophy**:
A trophy record that the owner's launcher marked as secret because the game hides it. Nobody but the owner sees, counts or hears of it until the owner reveals it; it then behaves like any record.

**Rarity**:
The share of players with a title who earned a trophy, counted from trophy records.

**Run report**:
A player's answer to "How did it run?" for one title, game version and toolkit version: a verdict (does not start, menus, in game, playable) and the system. The game page shows the most reported verdict once three players answered.
_Avoid_: Compatibility rating, review

**Feed**:
Recent activity of a player and their friends: trophies, new friends, first starts of a game, parties. Answered with one of four fixed reactions, never with text.
_Avoid_: Timeline, wall

**Party**:
A group of up to 8 friends that stays together across games, with one chat and one voice channel. A party belongs to no game; a session does.
_Avoid_: Group, squad, session

**Party leader**:
The member of a party who invites, removes members and moves the party into a session. The longest-standing member leads when the leader leaves.
_Avoid_: Host, owner

**Voice connection**:
The encrypted connection between the net services of two party members that carries their voice. It runs on the same endpoint as their tunnel and takes the same direct or relayed path, but is a connection of its own. Only members the party names can hold one.
_Avoid_: Voice tunnel, voice channel (that is the party's one channel as the player sees it)

**Call**:
A party that rings. A player calls a friend; the friend's launcher rings for 30 seconds, and accepting joins the party, which has its voice on. A group call is the party's voice. A call that nobody answers is a **missed call**. See ADR 0026.
_Avoid_: Voice chat, ring (as a noun for the whole call)

**Share play**:
A player streams their running game to one friend in their party, who watches, plays as them, or plays beside them. The friend needs neither the game nor its files.
_Avoid_: Remote play (playing one's own console from afar), broadcast

**Share screen**:
A player streams the game they are playing to the members of their party, who each choose whether to watch. Watch only: nothing comes back, and no one plays. It needs no invitation and no record in the cloud. Not share play, where one friend is invited and may take a pad.
_Avoid_: Screen share (the order of the words is the button's), broadcast, go live, share play (that is the one-guest feature with a pad)

**Streamer**:
The player whose game a share screen shows. The streamer starts and stops it and sees who watches. Not the host of a share.
_Avoid_: Host (that is share play's), broadcaster

**Viewer**:
A party member who chose to watch a streamer's share screen, in the share worker's window. A viewer can leave at any moment and never sends input.
_Avoid_: Guest (that is share play's), spectator

**Share**:
One run of share play between a host and a guest, from the invitation to the stop.
_Avoid_: Session (that is a group of players in a game), call

**Host**:
The player whose PC runs the game in a share. The host starts it, picks the mode, and can end it or take control back at any moment. A host is not necessarily the party leader.
_Avoid_: Streamer (who broadcasts to many), owner

**Guest**:
The party member the host invited into a share. A guest sees and hears the game and, when the mode allows, plays with a pad. A guest never gets the game or its data.
_Avoid_: Visitor, viewer (the viewer is the guest's window), spectator

**Share mode**:
What the guest may do in a share. Watch: see and hear only. Hand over: the guest plays as the host's pad, and the host's own pad is muted. Play together: the guest is a second player.
_Avoid_: Permission level, role

**Guest pad**:
The pad that the host's game sees for the guest's input. It exists only while the share mode allows it, and it is never more than buttons, sticks and triggers.
_Avoid_: Remote control, virtual keyboard

**Override**:
The host's way to take control back from the guest at once: a chord on their own pad, a key, or a button in the launcher. A second override ends the share.
_Avoid_: Kill switch, pause

**Stream**:
The picture and the sound of the host's game as they go to the guest during a share.
_Avoid_: Broadcast, recording, feed (that is the friends' activity list)

**Stream delay**:
The time from the host's game showing a picture to the guest's screen showing the same picture.
_Avoid_: Lag, ping

**Share connection**:
The encrypted connection between the net services of host and guest that carries the stream and the guest's pad. It runs on the same endpoint as their tunnel and takes the same direct or relayed path, but is a connection of its own. Only the two accounts the share names can hold it.
_Avoid_: Stream tunnel, call

**Share worker**:
The helper program on a player's PC that, for a host, turns the game's pictures and sound into the stream and, for a guest, shows it and reads the guest's pad. It is downloaded by the launcher the first time it is needed.
_Avoid_: Encoder, plugin, viewer (that is only the guest's side)

**Frame ring**:
The block of shared memory on the host's PC that the share worker makes and the game's engine fills with the pictures it presents and the sound it plays. One writer, one reader, no locks; the engine never waits for the worker. It is named at random for each share and only the user who runs the game can open it.
_Avoid_: Capture buffer, shared texture

**Pad state**:
One complete reading of the guest pad: fifteen buttons, two sticks and two triggers, and nothing else. The guest's viewer sends them, the host's net service checks and rate-limits them, and the engine plays them as a pad.
_Avoid_: Input event, key press

**Clip**:
The last seconds of a player's running game, kept in memory by the share worker while Clips is on and saved as an MP4 file when the player asks. It stays on their PC. Not a share screen (that goes to the party) and not a recording (which would run from a start to a stop).
_Avoid_: Recording, capture, replay

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

**Reply**:
A message that quotes an earlier message of the same conversation. The quote is a copy of the first 140 characters, marked when the quoted message was unsent.
_Avoid_: Thread (a reply starts no thread), forward

**Reaction**:
One of six fixed names an account puts on a message: Nice, Love, Laugh, GG, Wow, Sad. One per account and message. Not the feed's four reactions, which belong to feed items.
_Avoid_: Emoji, sticker, like

**Edit**:
A change of the text of one's own message within 15 minutes. The message shows "edited"; no client can read the earlier text.
_Avoid_: Revision (that counts changes to a conversation), update

**Unsend**:
The writer removes the text of their own message for everyone. A marker with the same number stays. Not leaving a conversation, not a moderator removing a message.
_Avoid_: Delete (deleting an account deletes messages), retract, recall

**Message revision**:
The counter of a conversation that rises with every edit, unsend and reaction. A client asks for what changed after the revision it holds, as it asks for messages after the number it holds.
_Avoid_: Version, etag

**Delivered**:
A direct message the other account's device has fetched. Between sent and read. Only direct conversations have it.
_Avoid_: Received, seen (groups say seen by)

**Mute (a conversation)**:
The player's own setting: no toast, no badge, no notification for that conversation. Not a moderator's mute of an account, which is a restriction.
_Avoid_: Silence, snooze

**Pin**:
The player's own setting: the conversation stays at the top of the list. At most five.
_Avoid_: Favourite, star

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
