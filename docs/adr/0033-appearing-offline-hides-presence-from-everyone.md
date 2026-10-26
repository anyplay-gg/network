# Appearing offline hides presence from everyone

_Renumbered from 0028 on 2026-10-26._

A player can set their account to appear offline. Friends then see them as offline, with no game and no joinable session, while the player stays signed in, keeps playing and joining, and keeps receiving messages and invitations. The setting is one boolean on the account, written with `PUT /v1/me/appear-offline` ([cloud API](../../spec/cloud-api.md#appearing-offline)).

## Decision

- **Everyone, not per friend, in v1.** The setting hides presence from every account, friends included. A list of friends who may still see the player is a second setting that can sit next to this one later, and a client that only knows the boolean keeps working.
- **On the account, not the device.** The cloud stores it, so every device has it and it survives sign-ins. A per-device setting would show a player online or offline depending on which PC they started last, which is the opposite of what they asked for.
- **A setting of its own, not a field of `Privacy`.** `PUT /v1/me/privacy` replaces all six audiences, so a launcher that only has a toggle would have to read and write back settings it does not show. Presence is also not an audience of a profile part: it has no "everyone" or "nobody".
- **The cloud hides it, the net service does not.** The cloud applies the setting where it hands presence to other accounts (friend lists, the `presence` event, the joinable session in an activity). The net service keeps the event connection, so the player is reachable for invitations and messages. A client that hid itself by closing its connection would also lose those.
- **The player is not hidden from themselves.** The state stays online and the client shows an "Appearing offline" hint, so nobody wonders why friends do not answer.

## Considered options

- Per friend from the start: the better privacy control, but a list to manage in the launcher and the portal before there is a launcher toggle at all.
- A value of `Presence` the player sets ("busy", "invisible", "away"): more room for later, but friends would see a state the player did not choose, and it needs a design for every state.
- Closing the event connection while "offline": no cloud change, but the player stops receiving invitations and messages live, which is the point of staying signed in.

## Consequences

- A friend cannot tell an account that appears offline from one that is offline, and cannot tell it from a missing reply to an invitation either. That is the purpose; it is not a block.
- Members of a session or a party still see each other, since they chose to play together.
- Games that read friends' presence through the bridge get the same lists, so they see the account offline too.
