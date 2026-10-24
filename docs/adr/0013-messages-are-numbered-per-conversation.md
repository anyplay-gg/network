# Messages are numbered per conversation

Every message gets a number from its conversation: 1, 2, 3 and so on. The cloud takes the next number with an update of the conversation's row, in the same transaction that stores the message, so the writers on every node of the cloud take turns on that row. A number is never used twice, and a message with a lower number is always committed before one with a higher number. A client keeps the highest number it has fetched for each conversation and asks for what comes after it, so a missed event, a reconnect or a second device loses nothing and needs nothing but that one number. Events are only pointers: conversation and number, no text.

The price is that all writers of one conversation queue on one row. A party has 8 members and a group 32, each limited to 30 messages a minute, so a conversation never gets near what a database does on one row; conversations do not wait for each other.

## Considered options

- Times as the order: nodes disagree by milliseconds, two messages can share a time, and a client cannot tell whether it missed one that arrives late.
- One counter for the whole cloud (a database sequence): it hands out numbers in the order it was asked, not the order of commits, so a message with a lower number can become visible after one with a higher number, and a client that fetched "after N" skips it. It is also one hot spot for every player.
- Numbers per recipient: a group message would be written once for each member, and the cloud's storage and the conversation's order would depend on how many members it had.
