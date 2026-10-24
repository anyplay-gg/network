# Social

What players do together on the network besides playing a game: profiles, trophies, a feed, messages, parties with voice, and sharing a game with a friend. The words are in [CONTEXT.md](../CONTEXT.md); the decisions in [docs/adr](adr), from [ADR 0009](adr/0009-trophy-records-not-trophy-texts.md) on.

The social layer runs in the same three places as the rest of the network. The cloud keeps what has to be shared and nothing more. The net service carries live traffic, voice and shared games, through the same tunnels as game traffic. The launcher shows it all, and reads what only the player's own PC may read: their games, their trophies, their playtime.

## Ground rules

1. **No game content in the cloud.** A trophy's name, text and picture come from the game's own files. They stay on the PCs of players who own the game. The cloud knows only which trophy, by number, of which title someone earned and when ([ADR 0009](adr/0009-trophy-records-not-trophy-texts.md)). The same holds for game art and icons.
2. **Our own trophies, nobody else's IDs.** Trophies are the network's record of what a player did in a game they own. They use the title ID the network knows already and the trophy's number in the game; they never use a platform's trophy set IDs, and nothing is ever sent to a platform.
3. **Private by default.** A new profile shows itself to friends only. Messages from people who are not friends wait in a request inbox, and links in them stay inactive. Each player chooses what friends and everyone else may see ([ADR 0012](adr/0012-private-by-default.md)).
4. **Live traffic is peer to peer.** Voice and shared games go through the tunnels, end to end encrypted, through a relay where needed. A player who hides their IP address keeps it hidden here too ([ADR 0010](adr/0010-voice-and-shared-play-go-through-the-tunnels.md)).
5. **Moderation from the first message.** Everything one player can send another can be reported, blocked and removed, and every moderation step is written down.

## Features

### Profile

A handle, an avatar, a short text, favourite games and a visibility for each part: friends, everyone, or nobody. The gamer card is the short form that friends see in their list: avatar, handle, what they play, their trophy count. Avatars are pictures the player uploads, scaled and checked by the cloud; they are not taken from games.

### Trophies and playtime

The launcher already reads a game's trophy list from the dump and the trophies the player earned from the records AnyPS5 writes while the game runs. It sends the cloud a **trophy record** for each earned trophy: title ID, trophy number, grade (bronze, silver, gold, platinum) and the time. Playtime goes the same way: title ID and minutes per day.

From the records of all players the cloud counts each trophy's **rarity**: the share of players with the title who earned it. A friend who owns the game sees names and pictures from their own copy; one who does not sees grades, counts and rarity.

Later, when AnyPS5 builds report trophy unlocks through the bridge, the net service records them the moment they happen, without waiting for the launcher to read the files.

### Feed

Activity of friends, newest first: a trophy earned, a game started for the first time, a session opened to friends, a profile change. Items are short and point to the thing they are about; the cloud keeps them 30 days. Players choose what of theirs appears there.

### Messages

One-to-one and group conversations as text, with read marks. A conversation between friends opens at once; a message from anyone else lands in the receiver's **message requests** until they accept it. The cloud stores messages to deliver them to every device and keeps them for a limited time ([ADR 0011](adr/0011-messages-are-stored-to-be-delivered.md)). New accounts may not write to strangers during their first day.

### Notifications

Friend requests, invitations, messages, party invitations and trophies of friends, as events on the existing event connection while the launcher runs. Pushing them to a phone comes with the phone app.

### Party

A group of friends, up to 8, that stays together across games and game sessions: one party chat, one voice channel, and a way to move everybody into the same session. Unlike a session, a party belongs to no game. The leader invites; members may leave at any time.

### Voice

Party members talk over the tunnels the net service already opens: Opus frames, one tunnel to each other member, no media server. Push-to-talk or open microphone, mute per member, and a volume per member. In-game voice of games that use the platform's voice libraries can come later through the bridge.

### Share play

A player streams their running game to a friend, who watches and, when the host allows it, takes over a pad. Video and pad input go through the tunnel between the two. The friend needs neither the game nor its files; the host decides and can stop at any time.

### Web profile

A page per profile at the cloud's address, showing only what the player made public: avatar, handle, gamer card and trophy counts. It never shows game art.

### Sign-in

Besides the e-mail code: passkeys, and one-tap sign-in with Apple, Google or Discord, through the system browser.

### Moderation

A queue of reports with the reported content attached, actions with a stated reason (warning, removal, mute, ban), an audit log of every action, and an appeal for the player concerned.

## Phases

| Phase | Content | Done when |
|---|---|---|
| S1 Profiles and trophies | profiles with visibility, avatars, trophy records, playtime, rarity, gamer cards in the friend list | two friends see each other's gamer cards and trophies, names from their own copies |
| S2 Parties and messages | parties, party chat, one-to-one and group messages, message requests, notifications | a party of three chats and moves into one session together |
| S3 Voice | party voice over the tunnels, push-to-talk, mute and volume per member | a party of four talks with relay only switched on |
| S4 Feed and boards | feed, trophy rarity in the feed, leaderboards of trophy counts and playtime among friends | the feed shows a friend's trophy within seconds |
| S5 Share play | streaming a game to a friend, taking over a pad | a friend without the game plays a level of it |
| S6 Web and sign-in | web profiles, passkeys, Apple, Google and Discord sign-in, moderation queue | a public profile opens in a browser; a player signs in with a passkey |

The infrastructure work in [anyplay-gg/cloud](https://github.com/anyplay-gg/cloud/blob/main/docs/ARCHITECTURE.md) runs beside these phases; every feature here is built for the cluster from its first line.
