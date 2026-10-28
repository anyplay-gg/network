# Friends play what runs: share play first

The network's aim is the best way to play AnyPS5 games with friends. The order of the work follows what reaches players soonest, for every game that runs, inside the line of [ADR 0021](0021-the-line-is-drawn-at-the-law.md):

1. **Share play** ([spec](../../spec/share-play.md)): watch, take over and play together as a second player by streaming. It needs no work per game, so it reaches every game AnyPS5 runs, and it is polished first.
2. **Virtual LAN**, proven with a game that has a LAN mode and needs no account.
3. **Engine compatibility** where a game stops before the point where people could play it together: a fix there helps every player of that game, online or not.
4. **Platform sessions** for games whose online play is peer to peer through the platform (phase 4 of the [roadmap](../DESIGN.md#roadmap)): when a candidate runs far enough to observe it.

Games whose multiplayer needs a publisher's account, the publisher's servers or anti-cheat are out of scope. ADR 0021 already keeps them offline on the PlayStation side; this decision also stops spending work on them as test cases.

## Considered options

- **Online play of the big titles first.** Rejected: on 28 October 2026 Minecraft Bedrock showed the pattern. Its multiplayer, LAN included, waits on a Microsoft sign-in that needs a platform authorization code only Sony can issue, so no answer of ours unlocks it.
- **Class A titles first.** Kept as step 4, not dropped: the candidates do not run far enough yet (Sackboy stops on a black screen), so the work would wait on the engine either way.

## Consequences

- Share play features (the second player, the guest's controller, the invitation flow) rank above new platform calls in the board.
- A game is a test case only if it runs far enough on AnyPS5 to show the feature being tested.
- The first "played together over the internet" demo is share play; phase 4's goal stays as written.
