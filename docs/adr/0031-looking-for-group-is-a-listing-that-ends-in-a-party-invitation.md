# Looking for group is a listing that ends in a party invitation

A player who wants company in a game posts a short listing: the game, how (online, LAN or Play Together), a language, a note of at most 140 characters and how many players they look for. It lives for one hour. Other players who have the game see it, ask to join, and the poster accepts. Accepting sends the player an invitation to the poster's party, and from there the network does what it already does: voice, invitations to the game, Play Together.

**Why a listing and not a lobby, a matchmaker or a forum.** The cloud has sessions and matchmaking tickets for what a game itself asks for ([DESIGN.md](../DESIGN.md)), and those are shaped by a game's calls. A listing is for people, not for a game's code. It names no session, creates nothing the game sees, and ends in a party, which every title already works with. That keeps it generic for all games: the cloud stores a title ID it never interprets, and per-title facts such as which modes a game supports stay in the launcher's data.

**Ownership is a filter, not a check.** The cloud never sees a player's games ([DESIGN.md](../DESIGN.md), rule 7). Listings are shown only for one `title` at a time, which the launcher asks for only for games in the player's library. A player who lies sees listings for a game they do not own and gains nothing the cloud protects. Nobody can browse all listings.

**Accepting needs no friendship.** The invitation an accepted request produces is the one case where a party invitation reaches a non-friend. The recipient asked for it, the poster chose it, and the usual party limits (leader only, a full party, a block) still hold. Declining is silent, so a decline is not a message that can be turned into harassment.

**Moderation from the first day.** A listing is public text from one account, so it carries the rules of a profile text (no links, no control characters), is rate limited, can be reported (the report keeps the note), and is hidden by a block in either direction. A muted account cannot post or ask; a ban removes the account's listings and requests. Listings are deleted a day after they close.

## Considered options

- **A lobby per game that players sit in.** More like a console, but it needs presence per game, which the cloud does not know.
- **Posting to friends only.** Safe, and it is what the feed and invitations already do. It does not help the player who has no friends who own the game, which is who this is for.
- **Auto-join on ask.** One less step, and one more way for a stranger to enter a party. The poster decides.
- **Matching by the cloud (queue, region, skill).** Possible later; a listing is the simplest thing that works for every game.

## Consequences

- New objects and paths in the [cloud API](../../spec/cloud-api.md#looking-for-group); new events `lfg_request` and `lfg_accepted`; `Report` gains `listing`.
- One more kind of public text to moderate, and one more table in the cloud's privacy inventory.
