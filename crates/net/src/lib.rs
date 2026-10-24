//! The AnyPlay net service: the program on a player's PC that holds the account, talks to the
//! cloud and carries game traffic to other players.
//!
//! The launcher starts it and talks to it over its standard streams (`spec/control-protocol.md`);
//! a game's bridge connects to it over loopback (`spec/bridge-protocol.md`). See
//! `docs/DESIGN.md` for how the parts fit together.

pub mod bridge;
pub mod cloud;
pub mod credentials;
pub mod events;
pub mod observe;
pub mod service;
pub mod traffic;
pub mod tunnel;

pub use service::{Config, run};

/// Installs the TLS crypto provider every connection of the process uses. Call once at start.
pub fn install_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
