# Voice and shared play go through the tunnels

Party voice and share play travel through the same iroh tunnels as game traffic: Opus frames for voice, one tunnel to each other member of a party of at most 8, and video and pad input between the two players of a shared game. Encryption, hole punching, relays, ticket checks and the relay-only setting then work for them as they do for games, and the network runs no media server. Eight members send to seven others at about 32 kbit/s each, well within a home connection.

## Considered options

- WebRTC with STUN and TURN: the usual choice for browsers, but a second stack beside iroh, with its own relays, credentials and IP exposure rules to keep in line with the first.
- A media server that mixes voice: one stream per member instead of seven, but every party's audio would pass through and be readable on AnyPlay's servers, and the servers would cost CPU per minute spoken.
