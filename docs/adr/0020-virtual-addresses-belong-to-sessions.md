# Virtual addresses belong to sessions

A virtual address is given to a member of a session, not to an online account. The cloud picks it when the account joins the session, it differs from the address of every other member of that session, and it stays the same until the account leaves. Before, the cloud leased one address of `198.18.0.0/15` to each online account, so no more than about 131,000 accounts could be online at once, whether or not any two of them ever met. A game only tells the members of its own session apart, and a session holds at most 64, so the range is now a pool every session draws from on its own. A cloud has no limit on players online from addressing any more.

## How an address is chosen

- **The account's own address, when free.** Every account has one preferred address, a hash of its ID inside the range (skipping addresses that end in .0 or .255). A session gives a joining account that address when no member holds it, else the next free assignable address after it. Two sessions therefore give the same player the same address nearly always, and `welcome.address` names that preferred address.
- **Chosen under the session's lock.** The cloud reads the members and adds the new one in the transaction that locks the session, so two nodes admitting two players at once cannot pick the same address; a unique index on the session and the address backs it.
- **Stable inside a session.** Joining again from another device or after a new endpoint changes nothing. Leaving and joining again may give another address, which the net service reports as the player leaving and arriving.
- **Always enough.** The most a session can hold (64) is far below the assignable addresses (130,048), so a join never fails for want of an address. The API crate asserts this at compile time.

## Players in several sessions, and parties

An account may be in several sessions (the cloud allows 4), and nothing makes two sessions agree on addresses. One peer in two sessions usually has the same address in both through the preferred address, but two different peers can hold one address in two sessions. A game tells peers apart by address, and so does the tunnel table, so the net service keeps one address per peer account over all sessions: the cloud's address for the first session that names the peer, and the next free one if another peer already holds it. A peer keeps that address as long as it is in any session the PC holds. It is also never the player's own address. This happens only after a collision of two hashes between peers one player meets in two sessions at once, and costs nothing otherwise.

A party has no addresses: voice goes by endpoint ([ADR 0017](0017-voice-has-a-connection-of-its-own.md)). The members of a party moving into a session join it one by one like anyone else.

## The player's own address

The bridge learns it from the `state` message, which the net service sends again whenever it changes. It is the player's address in the session the launcher shows (the running game's newest), else the preferred address from `welcome`. In nearly all cases these are the same, so nothing changes during a game; when they differ, the game gets a new `state` as it enters the session.

## Compatibility

`welcome.address` and `Member.address` keep their names and types. Nothing in the API is versioned, because old and new meet without trouble:

- **Old net service, new cloud.** It takes its own address from `welcome` (the preferred address, which is the one it has in a session unless another member holds it) and its peers' from the member list. Its tunnel table is one table over all sessions, so a collision across two sessions would send packets to the wrong tunnel; that needs two peers whose hashes collide (1 in 131,000 per pair) in two sessions of one player.
- **New net service, old cloud.** It takes the member list as it is; the addresses are unique there too.
- **Cloud nodes of both versions at once** (a rolling update). They share the store. The leases table stays in the schema for this and is not written any more; a later migration drops it once no node of the old version runs. An old node that admits a member into a session whose new members hold the leased address meets the unique index and answers with an error, and the client joins again. The window is the length of the update.

## Considered options

- **Keep one address per account and make the range larger.** There is no larger private range that games treat as local: `198.18.0.0/15` was chosen because it is reserved for testing and never routed ([DESIGN](../DESIGN.md)). Using `10.0.0.0/8` would collide with the player's own network.
- **Let each net service name its peers itself.** The cloud would hold no addresses at all. But a game may pass an address to another peer in its own messages, and then every member must call a player by the same address; a cloud-chosen address per session gives that, a per-PC one does not.
- **An address per pair of players.** Never collides, but the game would see a different address for one player on each PC, with the same trouble.
- **A global address per account in a bigger pool, kept for hours.** What the leases did. They also needed a sweep, an expiry and a rule for accounts that go offline, all gone now: the address lives and dies with the member row.
