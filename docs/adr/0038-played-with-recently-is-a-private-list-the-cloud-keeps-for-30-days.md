# Played with recently is a private list the cloud keeps for 30 days

A player who had a good game with a stranger has no way back to them: the session is gone and the name was in a member list for an hour. The cloud already knows who was in which party and session, so it can keep the pairs that mattered: two accounts that were members of the same party or session at the same time for at least two minutes. The player sees them in "Played with recently" and can add a friend, open a profile or block from there ([cloud API](../../spec/cloud-api.md#played-with-recently)).

## Decision

- **Private to the account.** Only the account reads its own list. The other person is never told, and nothing about it reaches a profile, card, feed or page. Seeing who you played with is a convenience for you; being on someone's list must never be a signal to anyone.
- **Two minutes, 30 days, 50 people.** Two minutes keeps out the player who joined the wrong session and left. Thirty days and 50 entries bound what the cloud holds about anyone: old entries and the oldest of a long list go.
- **One row per direction.** Each account keeps its own entry for the other. That makes the 50-person limit, the player's own switch and "clear" local to one account, and clearing a list never edits someone else's.
- **The cloud samples, it does not hook departures.** A job that runs about once a minute looks at who is together in a party or a session right now and for how long, and records the pairs over two minutes. There are many ways to leave (leaving, removal, a closed session, a net service that stays disconnected); a sampler sees all of them without a line in each, and it also catches people who stay together for hours. The price is up to a minute of delay.
- **A setting of its own.** "Remember people I play with" is on by default and is written with its own call, like appearing offline ([ADR 0033](0033-appearing-offline-hides-presence-from-everyone.md)): `PUT /v1/me/privacy` replaces all six audiences with defaults for what is left out, so an older client saving the audiences would switch a deliberate "off" back on. Off stops new entries for this account; it does not clear what is there, and "clear" is a separate action.
- **Clearing is not undone by the sampler.** The cloud keeps the time of the last clearing per account, and only a pair that came together after it is recorded again. Without that, a player who clears the list in the middle of a party would see the party back a minute later.
- **Blocks hide, and stop recording.** An entry with a block in either direction is left out of the answer and none is recorded while the block lasts. It is not deleted, so unblocking within 30 days brings it back.
- **Generic.** The cloud stores the title ID and title the session carried, as it does for presence, and interprets neither.

## Considered options

- **Hook every departure.** Exact times, but five code paths and the long-lived party that nobody leaves is never recorded.
- **Record every session a pair shared, as a history.** More to show, and a map of whom a person plays with and when. A single `last` is enough for the use.
- **One shared row per pair.** Half the rows, but one side's switch or clear would have to edit the other's data.
- **Show it to the other person ("X played with you").** Useful for a thank-you, and exactly the signal this list must not be.

## Consequences

- One more table in the cloud's data inventory and a line in the privacy notice. The list is deleted with either account and is part of the export.
- A player who turns remembering off still appears in the lists of others who have it on, from the sessions they shared. That is their own record of their own games.
- The cloud runs one more periodic job, bounded by the size of parties (8) and sessions (64).
