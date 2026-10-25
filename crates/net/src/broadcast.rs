//! Broadcasts of a game on the virtual LAN: what counts as one, how much of it one player may
//! send, and what has been counted.
//!
//! A game that looks for others on a LAN sends a datagram to the limited broadcast address or to
//! the broadcast address of its subnet. The bridge hands such a datagram to the net service like
//! any other (`spec/bridge-protocol.md`, "Broadcasts"); `traffic` copies it into the tunnels of
//! the other members of the player's current session. This module holds the decisions that do not
//! need a tunnel, so they can be tested alone.

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyplay_api::VIRTUAL_NETWORK;

/// The largest game payload of a broadcast. Bigger ones are dropped: a copy per member of a big
/// datagram would turn one packet into dozens, and discovery packets are small.
pub const LARGEST_BROADCAST: usize = 1200;
/// Broadcasts one player may send per second, over a minute.
pub const BROADCASTS_PER_SECOND: u32 = 40;
/// How many broadcasts may go out at once before the rate applies.
pub const BROADCAST_BURST: u32 = 80;
/// The most game bytes of broadcasts one player may send per second, over a minute.
pub const BROADCAST_BYTES_PER_SECOND: u32 = 32 * 1024;

/// The broadcast address of the virtual subnet (`198.19.255.255` for `198.18.0.0/15`).
pub fn subnet_broadcast() -> Ipv4Addr {
    let (network, prefix) = VIRTUAL_NETWORK;
    let host = u32::MAX >> prefix;
    Ipv4Addr::from(u32::from(network) | host)
}

/// True for the two destinations the net service treats as a broadcast: `255.255.255.255` and
/// the broadcast address of the virtual subnet.
pub fn is_broadcast(address: Ipv4Addr) -> bool {
    address == Ipv4Addr::BROADCAST || address == subnet_broadcast()
}

/// What to do with a broadcast datagram.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Send,
    TooLarge,
    TooFast,
}

/// Two token buckets, one counting datagrams and one counting bytes. A broadcast passes when both
/// hold enough.
#[derive(Debug)]
pub struct Limiter {
    datagrams: f64,
    bytes: f64,
    at: Instant,
}

impl Limiter {
    pub fn new(now: Instant) -> Self {
        Self {
            datagrams: f64::from(BROADCAST_BURST),
            bytes: f64::from(BROADCAST_BYTES_PER_SECOND) * 2.0,
            at: now,
        }
    }

    /// Decides on a broadcast of `len` game bytes at `now`, and takes its share when it passes.
    pub fn admit(&mut self, now: Instant, len: usize) -> Verdict {
        if len > LARGEST_BROADCAST {
            return Verdict::TooLarge;
        }
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.at = now;
        self.datagrams = (self.datagrams + elapsed * f64::from(BROADCASTS_PER_SECOND))
            .min(f64::from(BROADCAST_BURST));
        self.bytes = (self.bytes + elapsed * f64::from(BROADCAST_BYTES_PER_SECOND))
            .min(f64::from(BROADCAST_BYTES_PER_SECOND) * 2.0);
        // Count the header too, so that empty datagrams are not free.
        let cost = (len + 8) as f64;
        if self.datagrams < 1.0 || self.bytes < cost {
            return Verdict::TooFast;
        }
        self.datagrams -= 1.0;
        self.bytes -= cost;
        Verdict::Send
    }
}

/// What the net service did with broadcasts since it started. The counts name no one.
#[derive(Debug, Default)]
pub struct BroadcastStats {
    /// Broadcasts accepted from the game.
    pub accepted: AtomicU64,
    /// Copies put into tunnels (one broadcast makes one copy per member that can be reached).
    pub copies: AtomicU64,
    /// Broadcasts with no member to send to.
    pub alone: AtomicU64,
    pub too_large: AtomicU64,
    pub too_fast: AtomicU64,
}

/// A copy of the counters at one moment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BroadcastTotals {
    pub accepted: u64,
    pub copies: u64,
    pub alone: u64,
    pub too_large: u64,
    pub too_fast: u64,
}

impl BroadcastStats {
    pub fn totals(&self) -> BroadcastTotals {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        BroadcastTotals {
            accepted: get(&self.accepted),
            copies: get(&self.copies),
            alone: get(&self.alone),
            too_large: get(&self.too_large),
            too_fast: get(&self.too_fast),
        }
    }

    pub(crate) fn add(counter: &AtomicU64, by: u64) {
        counter.fetch_add(by, Ordering::Relaxed);
    }
}

/// How often a summary of the counters goes to the log, when they changed.
pub const SUMMARY_EVERY: Duration = Duration::from_secs(60);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_broadcast_addresses_count() {
        assert!(is_broadcast(Ipv4Addr::BROADCAST));
        assert!(is_broadcast(Ipv4Addr::new(198, 19, 255, 255)));
        assert!(!is_broadcast(Ipv4Addr::new(198, 18, 255, 255)));
        assert!(!is_broadcast(Ipv4Addr::new(198, 18, 0, 2)));
        assert!(
            !is_broadcast(Ipv4Addr::new(224, 0, 0, 251)),
            "multicast is not carried"
        );
        assert!(
            !is_broadcast(Ipv4Addr::new(192, 168, 1, 255)),
            "nor a real network's"
        );
    }

    #[test]
    fn a_burst_passes_then_the_rate_applies() {
        let start = Instant::now();
        let mut limiter = Limiter::new(start);
        for _ in 0..BROADCAST_BURST {
            assert_eq!(limiter.admit(start, 20), Verdict::Send);
        }
        assert_eq!(limiter.admit(start, 20), Verdict::TooFast);
        // A quarter of a second brings ten datagrams back.
        let later = start + Duration::from_millis(250);
        for _ in 0..10 {
            assert_eq!(limiter.admit(later, 20), Verdict::Send);
        }
        assert_eq!(limiter.admit(later, 20), Verdict::TooFast);
    }

    #[test]
    fn bytes_are_limited_too() {
        let start = Instant::now();
        let mut limiter = Limiter::new(start);
        let mut sent = 0;
        while limiter.admit(start, LARGEST_BROADCAST) == Verdict::Send {
            sent += 1;
        }
        assert!(
            sent < BROADCAST_BURST,
            "the byte budget ran out first: {sent}"
        );
        assert!(sent >= 40);
    }

    #[test]
    fn large_datagrams_are_refused_whatever_the_budget() {
        let mut limiter = Limiter::new(Instant::now());
        assert_eq!(
            limiter.admit(Instant::now(), LARGEST_BROADCAST + 1),
            Verdict::TooLarge
        );
        assert_eq!(
            limiter.admit(Instant::now(), LARGEST_BROADCAST),
            Verdict::Send
        );
    }
}
