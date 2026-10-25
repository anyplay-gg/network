# Trophy points and levels are our own

Every trophy record is worth **trophy points** by its grade, and a player's points make a **trophy level** from 1 to 100. Both are computed from trophy records alone, so the cloud and every client get the same numbers and no game content is needed ([ADR 0009](0009-trophy-records-not-trophy-texts.md)).

| Grade | Points |
|---|---|
| Bronze | 10 |
| Silver | 25 |
| Gold | 60 |
| Platinum | 150 |

Level `L` starts at `6 * (L - 1) * (L + 10)` points. Level 1 starts at 0, level 2 at 72, level 3 at 156, level 10 at 1,080, level 50 at 17,640 and level 100 at 65,340. Each step is 12 points longer than the one before: 72, 84, 96 and so on up to 1,248 points from level 99 to 100. The cloud sends `{points, level, from, to}`, so a client can draw a progress bar without knowing the formula.

## Why these numbers

- **The grade ratios follow how a game uses its grades.** A game gives many bronze, fewer silver, a handful of gold and one platinum for finishing the rest. A gold is worth 2.4 silver and 6 bronze; a platinum 2.5 gold. A typical game (20 bronze, 8 silver, 3 gold, 1 platinum) is worth 730 points, so platinum is the largest single prize but does not outweigh the work behind it (about a fifth of the game).
- **Whole numbers that end in 0 or 5**, so a player can read a trophy's worth and add up a game in their head.
- **The level curve grows slowly and linearly in its steps (quadratic in total), with a cap.** The first level takes about seven bronze trophies, so a new player levels up within the first evening. A player who completes about 90 typical games reaches level 100. A curve that grows by a fixed number has no jumps and no tables to keep in step with a client; the closed form is one line in any language.
- **The cap is 100** so the number stays short and a "maxed" state exists. Points keep counting after it.
- **No relation to any other platform's numbers or curve.** The values were chosen from the reasoning above and are ours to change. If they change, every client gets the new numbers from the cloud on its next request, because the cloud always sends points and level and clients only draw them.

## Rules that go with them

- **Points follow the trophies' visibility.** Anyone who may see a player's trophies ([ADR 0012](0012-private-by-default.md)) sees their points and level; for anyone else they are null. Points and level count only the records the viewer may see (below).
- **Secret trophies stay secret until their owner reveals them.** A game marks some trophies as secret. The launcher tells the cloud which numbers are secret and reveals one when the player chooses to. Until then the owner sees it, and no one else sees the record, counts it or gets a feed item for it. The owner's own view and everyone else's therefore can differ in counts, points and level; that is the price of keeping a secret.
- **Comparing two players** is a request for both players' records in one title, plus the totals. It shows nothing that the trophy list and profile do not already show, with the same visibility; it only puts them side by side so the client does not need two requests and a merge.

## Considered options

- **Points computed by the client only:** no new cloud code, but a profile viewed on the web or another client could not show them, and two clients could disagree.
- **Storing points per record:** a change of values would need a migration of every record. Computed from the grade, the values are one function.
- **A table of levels instead of a formula:** more freedom in the curve, but a table to ship and keep identical in every client.
- **Weighting by rarity:** rare trophies would be worth more, but a trophy's worth would change as other players earn it, and a rarity below ten players would reveal who earned it.
