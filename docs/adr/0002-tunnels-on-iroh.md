# Tunnels on iroh

Tunnels between players use [iroh](https://www.iroh.computer/) 1.0, whose wire protocol is stable since 15 June 2026. It gives us QUIC with hole punching, end-to-end encryption keyed to each device, and stateless relays that we host ourselves with bearer-token access, all from Rust under MIT or Apache-2.0. Its makers report that about nine in ten connections go direct, so relays carry only the rest.

## Considered options

- libjuice or libdatachannel (ICE): mature, but we would build TURN relays, encryption and reconnection ourselves.
- Valve's GameNetworkingSockets: proven in games, but C++ and it still needs our own signaling and relays.
- A WireGuard-style virtual LAN (Tailscale, ZeroTier): runs at the operating-system level, needs drivers or admin rights, and ZeroTier's controller is not under an open license.
