# The net service runs as its own process

The net service is a separate program that the launcher starts and watches, not a module inside the launcher. The launcher deliberately has no async runtime, while the net service needs tokio and iroh. As its own process it also keeps a player's presence online while no game runs, survives a game crash, and talks to the launcher over a local pipe, as two launcher instances already do.
