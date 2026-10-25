# Privacy and security

- **Encrypted end to end.** Tunnels are QUIC with keys bound to each device. A relay forwards bytes it cannot read.
- **Hidden addresses.** A player can keep every tunnel on the relay, so other players never learn their IP address (`--relay-only`, *Hide Your IP Address* in the launcher). The cloud publishes no player's direct addresses.
- **Relays admit only players.** A relay lets an endpoint in only with a ticket the cloud signed for that endpoint. It checks the ticket on its own and never calls the cloud ([ADR 0007](adr/0007-relays-check-signed-tickets.md)).
- **Loopback only.** The net service accepts bridge connections from loopback with a secret for one game start, and the launcher's channel is the process's own standard streams, so no other program can speak for the player.
- **Nothing from a platform holder.** No servers, code, keys, certificates or leaked documents. Formats come from what games send through the bridge, written down in [spec/](../spec) separately from the code.

Report a vulnerability privately; see [SECURITY.md](https://github.com/anyplaynet/.github/blob/main/SECURITY.md).
