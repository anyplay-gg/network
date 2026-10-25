# A call is a party that rings

Calling a friend makes a party of two with voice on. The call is an ordinary party invitation with a `call` flag: it lives 30 seconds instead of 15 minutes, and the callee's launcher rings while it is open. Accepting joins the party; declining or letting it run out ends it. A group call is the party's voice, and "add to call" is one more call invitation into the same party.

A call nobody answered is kept as a **missed call**, one row per caller and callee, until the callee has seen it. The caller is told when the call was declined or rang out (`call_ended`), so the launcher can drop its empty party.

## Considered options

- **A call object of its own** (`/v1/calls`, its own state, its own events): clearer words, but a second lifecycle next to the party, with its own members, expiry and blocks. Everything a call needs, who may be invited, blocks, the 8-member limit, the voice connections between the members, is already the party's.
- **Ringing in the net service only:** no cloud change, but a callee who is offline or whose launcher is closed would never know, and nothing could be shown as missed.

## Consequences

- Only the party's leader rings, as only the leader invites. In a 1:1 call that is the caller.
- A friend who is in a party already is not rung: the invitation would only fail on accept. The caller is told by an empty `sent`.
- Calls are not a way around the rules of messages: only friends are rung, a block in either direction stops it, a muted account cannot call, and calls are rate-limited per account.
