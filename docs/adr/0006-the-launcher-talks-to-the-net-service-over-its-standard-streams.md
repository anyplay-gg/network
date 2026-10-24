# The launcher talks to the net service over its standard streams

The launcher starts the net service as a child process and exchanges JSON lines over the child's standard input and output. Only the launcher holds those streams, so the channel needs no secret, no port and no pipe name, works the same on Windows and Linux, and needs nothing but a reader thread in a launcher without an async runtime. When the launcher closes, the net service sees its input end and stops, so a player who closes the launcher goes offline; a game that is still connected keeps it alive until the game ends.

## Considered options

- A named pipe or Unix socket, as between two launcher instances: works, but needs per-platform code, an access check and a name, all to reach a process the launcher started itself.
- A loopback port with a token, like the bridge: the bridge needs it because a game is not our child; the launcher does not.
