# Messages change in place, and a counter says what changed

A player can fix a typo, take a message back and answer with a reaction. All three change a message that already has its number ([ADR 0013](0013-messages-are-numbered-per-conversation.md)), so "ask for what comes after the number I hold" no longer finds them. Each conversation therefore has a second counter, its revision, that rises by one with every edit, unsend and reaction. It is taken in the same transaction and under the same row lock as the message numbers. A client holds the highest revision it has seen and asks for the messages that changed after it. A missed event, a reconnect or a second device loses nothing and needs one more number.

Three rules come with it.

- **Edit for 15 minutes, no history for readers.** The writer may change the text for 15 minutes. The message shows "edited" and no client can read an earlier text. A report saves the text at the moment it is filed (see Reports in the cloud API), but a writer could still send abuse and unsend it before the victim reports. So the text an unsend removes, and the text an edit replaces, is kept for seven days in a hold that only a report reads, then deleted. That is the narrowest copy that keeps the report system honest; keeping every version for ever would store text the player thought gone, for a case a week covers. A reply's quote is a live copy and changes with an edit, as the message it quotes does.
- **Unsend removes the text and keeps the message.** The text and the reactions are deleted from the store; the row stays with its number and time, marked deleted. Numbers do not skip, a reply can still show that what it quoted is gone, and the other devices learn about it through the same revision. It works at any time while the cloud still keeps the message, and also for a muted account: taking back words is never what a mute should prevent. A message unsent less than seven days ago can still be reported.
- **One reaction per account, six names.** The set is fixed and has no text, for the reason in [ADR 0024](0024-the-feed-has-four-reactions-and-no-comments.md): a reaction cannot carry a sentence. It is not the feed's set. A message shows who chose what, because a conversation has at most 32 members and the names are already shown there.

## Considered options

- A new message for every edit, unsend and reaction, with the existing numbers: the client code stays as it is, but a conversation fills with numbers that are not messages, a reaction takes the rate limit of a message, and "the newest message" stops being a message.
- A revision per message: the client would have to ask for each message it holds, or the cloud would send every message on every event.
- Delete the row on unsend: the numbers skip, replies point at nothing and a client cannot tell an unsend from a message it never received.
- An edit window without end, or none: without an end an old message can be turned into something else after others answered it; without edits, a typo stays.

## Consequences

- `Message` gains `reply`, `edited`, `deleted`, `reactions` and `rev`; `Conversation` gains `rev`, `unread`, `muted` and `pinned`. A cloud and a client that predate them still read each other: every new field is optional.
- A change to a message costs the same lock as writing one. The 60 per minute limit for changes is separate from the 30 messages.
- The export lists what an account wrote with the current text, and an unsent message without text.
