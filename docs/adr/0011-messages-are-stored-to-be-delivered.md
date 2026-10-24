# Messages are stored to be delivered

The cloud stores messages so that it can deliver them to every device of the receiver, including ones that are offline when the message is sent, and keeps them for a limited time. They travel encrypted, but not end to end: the cloud can read them. That lets a report carry the reported messages to moderation, which a network for players of all ages needs from its first day. End-to-end encryption for conversations between friends can follow as its own decision, once reporting works without the server reading along.

## Considered options

- End to end from the start: the cloud could not read messages, but reports would rest on what the reporter chooses to forward, and every device of an account would need its own keys before the first message.
- Delivery only to devices that are online, nothing stored: no storage, but a message to a friend who is offline would be lost.
