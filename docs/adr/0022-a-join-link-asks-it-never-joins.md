# A join link asks, it never joins

The launcher can show a **Join** button on the player's Discord profile, so that Discord friends, who are not always AnyPlay friends, can find their way into a game. The button holds a link, and a link on a public profile is seen by everyone who sees the profile, so the design starts from that.

- **The link holds a join code, nothing else.** The code is random and short-lived. It carries no account ID or handle, the cloud keeps only a hash, and the account can revoke it. The page behind it reveals the handle, the game's title and the number of players, and nothing more.
- **A code stands for the player, not for a session.** It opens the session the player offers when it is redeemed, else the party they lead. It keeps working when a game ends and the next one opens, until it expires, and it opens nothing when there is nothing to open. A launcher asks for a new code before the old one runs out and revokes it when the player stops playing.
- **Redeeming makes an invitation and nothing else.** It gives the person an ordinary session or party invitation from the player, past the rule that only friends are invited, and the launcher shows it as it shows every invitation: Join Game or Decline. Opening the website, pressing **Open in AnyPlay** and redeeming a code start no game and join nobody. The version guard still runs when the person presses Join.
- **Blocks hold.** A block between the person and the player or a member answers `not_found`, the same as for a code that does not exist.
- **The web page asks for a sign-in first.** A visitor who has no AnyPlay account is the one the link is for; the page takes them through creating one and back.

## Considered options

- **Discord's own join secrets** (`secrets.join` and the Join button of a rich presence). They show a Join button without leaving Discord, but the secret is handed to every friend's Discord client and the player's profile then cannot hold a URL button as well; a secret also cannot lead to a website for people without AnyPlay.
- **The code is the session ID.** Session IDs are not secrets and cannot be revoked; anyone who sees one in a log could ask to join.
- **Redeeming joins at once.** Faster, but a link in a chat would then put a player into a game without their asking.
