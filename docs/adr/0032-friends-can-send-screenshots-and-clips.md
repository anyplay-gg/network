# Friends can send screenshots and clips

A player can send a screenshot or a clip from their Captures to a friend, and it shows in the message thread as a media bubble. This is the first time the cloud stores something that is not text, and it is a departure from [ADR 0028](0028-clips-are-kept-by-the-worker-and-stay-on-the-pc.md), which keeps clips on the PC: a clip still never leaves the PC on its own. It leaves when the player presses Send to a Friend, once, to one friend.

[ADR 0011](0011-messages-are-stored-to-be-delivered.md) said that pictures need storage, scanning and a review path first. This decision gives each of them the smallest form that holds:

- **Storage.** The file goes to the cloud's disk behind an interface (`MediaStore`), not into the database. The object name is 128 random bits and means nothing. Access is by the account, never by the name: only the two members of a direct conversation can fetch a file. A cloud that wants object storage implements the interface.
- **Who.** Friends only, in a direct conversation both have accepted, with no block. Never a stranger, never a message request, never a group or a party chat. That is the narrowest audience that still serves "show my friend this", and it means every file has a recipient who knows the sender.
- **Size and type.** Screenshots (PNG, JPEG) up to 10 MB; clips (MP4) up to 100 MB and 60 seconds. The cloud checks the first bytes against the declared type, decodes images under limits and reads a clip's length. It does not transcode or scan content: the list of allowed types is short on purpose, and the files are shown only by the launcher, which decodes images itself and hands a clip to the system's player.
- **Expiry.** 30 days, the same as messages. The file is deleted by the sweep; it is not kept forever for a conversation that is long over.
- **Review.** Every media message can be reported by the recipient. A report keeps the file for as long as the report is kept, even if the sender unsends it before or after, so an unsend cannot erase the evidence (the same reason as the report hold for text in [ADR 0030](0030-messages-change-in-place.md)). Moderators read it through a route only they can use.
- **Take back.** The sender can unsend: access ends at once, and the file is deleted after a 7-day hold (longer if a report holds it), the same reason as the text hold in ADR 0030.

## Considered options

- **Peer to peer through the tunnels.** Nothing stored in the cloud, but both players must be online together, and there would be nothing for a report to hold.
- **Upload to a third-party host and send a link.** Pushes storage and review to a service we do not control, and links in messages are what the launcher keeps inactive.
- **Strangers and groups too.** Needs scanning and a larger review path than a small project has. Not now.
- **Re-encode every file.** Strips metadata and normalises, but costs a lot of CPU for 100 MB clips, and quality for screenshots. Not now; the files come from the launcher's own captures.

## Consequences

- `Message` gains an optional `media`; two new paths for sending and fetching; a moderator route.
- The cloud's privacy inventory and the privacy policy mention shared screenshots and clips. A cloud's `media_dir` is state: it needs backup, space, and one shared volume across nodes.
- A clip has no thumbnail in the thread: the cloud makes none, and the launcher shows a card.
