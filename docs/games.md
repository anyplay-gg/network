# Which games can play online

The network answers the platform's own services for online play with AnyPlay accounts. It can also bring back a game's online play after its publisher has shut the service down, with a server written from observation. It never stands in for a publisher service that still runs, and never issues tokens for anyone else's client. A game whose online play runs on such a live service stays offline ([the line is drawn at the law](adr/0021-the-line-is-drawn-at-the-law.md)).

| Class | The game's online part | On the network |
|---|---|---|
| A | Platform services only: sessions, matchmaking, peer-to-peer play, one player hosting | the goal |
| B | Platform services plus a publisher backend | only if the game also plays without the backend |
| C | A publisher backend only: own accounts, dedicated servers, live-service games | stays offline while the publisher runs it; a revival server only after the publisher has shut it down |
| 0 | No online part | nothing to do |

What is still missing for real games is listed in [gaps.md](gaps.md); how formats are learned is in [OBSERVING.md](OBSERVING.md).
