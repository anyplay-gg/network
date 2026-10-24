# Relays check signed tickets

A relay admits a net service only with a ticket that the cloud signed with Ed25519 for that net service's endpoint ID and that has not expired. The relay knows the cloud's public key and checks the ticket on its own, so it keeps no state, never calls the cloud and keeps working while the cloud restarts. A ticket names one endpoint, so a copied ticket is useless to anyone else. Tickets last 24 hours; a ban takes effect at the next renewal, and a banned account loses its sessions at once anyway, so nobody is left to relay to.

## Considered options

- One shared token for all players, as `iroh-relay` offers: anyone who reads it out of a net service could use our relays.
- A relay that asks the cloud about each connection over HTTP, also built into `iroh-relay`: every relay connection would depend on the cloud being up and fast.
