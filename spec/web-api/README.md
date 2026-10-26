# Platform web API groups

What games send to the platform's web API, group by group, as observed in games we own ([how games are observed](../../docs/OBSERVING.md)). A group the net service answers has a handler that links its file here. Every other group is answered with status 0, "not offered", which a game handles like being offline.

| Group | Status | Seen in |
|---|---|---|
| [`communicationRestrictionStatus`](communicationRestrictionStatus.md) | offered, shape unconfirmed | Overcooked! All You Can Eat |
| [`userProfile`](userProfile.md) | offered, shapes are guesses | Minecraft Dungeons II (signed out), Overcooked! All You Can Eat (signed in) |

## How requests reach the bridge

A game makes web requests through `libSceNpWebApi2`, `libSceNpWebApi` or `libSceNpCppWebApi`. Some games ship their own copy of `libSceNpCppWebApi` in `sce_module`. AnyPS5 relinks that copy and runs it as the game's own code, so its requests reach the bridge through `libSceNpWebApi2` (seen in Minecraft Dungeons II).

## A group's file

- **Status:** *observed* (the requests are known, the answer is not), *specified* (the answer the game accepts is known) or *offered* (the net service answers it).
- **Observed:** which game, which build, how long it ran, signed in or out.
- **Requests:** method, path with its placeholders, query, headers and body.
- **What the game did** with each answer it got.
- **Unknown:** everything the observations do not settle.

Title IDs may appear here; they never appear in code.
