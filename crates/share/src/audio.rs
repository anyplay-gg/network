//! The game's sound on the wire (`spec/share-play.md`, audio datagram): the sender's numbering
//! of Opus packets, and the viewer's jitter buffer that puts them back in order and says, for
//! every 10 ms the speakers ask for, what the decoder should do. It holds Opus packets, not
//! sound, so it is tested without a codec.

use std::collections::BTreeMap;

use crate::wire::{AudioDatagram, Refusal};

/// The sample rate of the game's sound on the wire.
pub const RATE: u32 = 48_000;
/// Stereo.
pub const CHANNELS: usize = 2;
/// Samples per channel in one packet: 10 ms.
pub const FRAME_SAMPLES: usize = 480;

/// The shortest wait before the sound plays: 40 ms.
pub const MIN_DEPTH: usize = 4;
/// The longest: 120 ms.
pub const MAX_DEPTH: usize = 12;
/// Packets the buffer may hold above its depth before it skips ahead to keep the delay.
const SLACK: usize = 4;
/// Ticks of concealment after which a stream that stopped is given up; it starts again with
/// the next packet.
const MAX_CONCEAL: u32 = 10;
/// Ticks without a late packet after which the depth shrinks by one packet.
const CALM_TICKS: u32 = 500;

/// Numbers the Opus packets of one stream and writes their datagrams.
#[derive(Debug)]
pub struct AudioPacketiser {
    epoch: u8,
    sequence: u16,
    timestamp: u32,
    interrupted: bool,
}

impl AudioPacketiser {
    pub fn new(epoch: u8) -> AudioPacketiser {
        AudioPacketiser {
            epoch,
            sequence: 0,
            timestamp: 0,
            interrupted: false,
        }
    }

    /// The sound stopped for a while (nothing captured): the next packet says so, and the
    /// viewer starts its buffer again.
    pub fn interrupt(&mut self) {
        self.interrupted = true;
    }

    /// The datagram of the next 10 ms packet.
    pub fn packet(&mut self, opus: &[u8]) -> Result<Vec<u8>, Refusal> {
        let datagram = AudioDatagram {
            interrupted: self.interrupted,
            epoch: self.epoch,
            sequence: self.sequence,
            timestamp: self.timestamp,
            payload: opus,
        }
        .encode()?;
        self.interrupted = false;
        self.sequence = self.sequence.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(FRAME_SAMPLES as u32);
        Ok(datagram)
    }
}

/// What the decoder does for the next 10 ms.
#[derive(Debug, PartialEq, Eq)]
pub enum Playout {
    /// Nothing to play: silence.
    Silence,
    /// Decode this packet.
    Packet(Vec<u8>),
    /// The packet due was lost; rebuild it from the redundancy in the next one, which plays on
    /// the next tick.
    Recover(Vec<u8>),
    /// The packet due is missing and nothing can rebuild it: let the codec fill in.
    Conceal,
}

/// What the buffer saw since it was made.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioStats {
    pub received: u64,
    /// Arrived after its time had passed.
    pub late: u64,
    /// Rebuilt from the next packet or concealed.
    pub lost: u64,
    /// Dropped to keep the delay down.
    pub skipped: u64,
    /// The current wait, in packets of 10 ms.
    pub depth: usize,
}

/// The viewer's jitter buffer.
#[derive(Debug)]
pub struct AudioJitter {
    /// Packets by sequence number counted on from the first one (no wrap).
    packets: BTreeMap<i64, Vec<u8>>,
    /// The sequence number of the first packet, to count on from across the wrap.
    base: Option<(u16, i64)>,
    epoch: Option<u8>,
    /// The packet due next once playing.
    next: Option<i64>,
    depth: usize,
    concealed: u32,
    calm: u32,
    stats: AudioStats,
}

impl Default for AudioJitter {
    fn default() -> Self {
        AudioJitter::new()
    }
}

impl AudioJitter {
    pub fn new() -> AudioJitter {
        AudioJitter {
            packets: BTreeMap::new(),
            base: None,
            epoch: None,
            next: None,
            depth: MIN_DEPTH,
            concealed: 0,
            calm: 0,
            stats: AudioStats::default(),
        }
    }

    pub fn stats(&self) -> AudioStats {
        AudioStats {
            depth: self.depth,
            ..self.stats
        }
    }

    /// Takes in one datagram. A new epoch or an interrupted stream starts the buffer again.
    pub fn insert(&mut self, bytes: &[u8]) -> Result<(), Refusal> {
        let datagram = AudioDatagram::parse(bytes)?;
        if datagram.interrupted || self.epoch != Some(datagram.epoch) {
            self.restart();
            self.epoch = Some(datagram.epoch);
        }
        let sequence = self.unwrap_sequence(datagram.sequence);
        self.stats.received += 1;
        if self.next.is_some_and(|next| sequence < next) {
            // Its time has passed: wait a packet longer from now on.
            self.stats.late += 1;
            self.depth = (self.depth + 1).min(MAX_DEPTH);
            self.calm = 0;
            return Ok(());
        }
        self.packets
            .entry(sequence)
            .or_insert_with(|| datagram.payload.to_vec());
        Ok(())
    }

    /// What to play for the next 10 ms.
    pub fn pop(&mut self) -> Playout {
        let Some(next) = self.next else {
            // Waiting until the buffer is as deep as it should be.
            let Some((&first, _)) = self.packets.first_key_value() else {
                return Playout::Silence;
            };
            if self.packets.len() < self.depth {
                return Playout::Silence;
            }
            self.next = Some(first);
            return self.pop();
        };
        // Too far behind the newest packet: drop the oldest to keep the delay.
        while self.packets.len() > self.depth + SLACK {
            if let Some((sequence, _)) = self.packets.pop_first() {
                self.stats.skipped += 1;
                self.next = Some(sequence + 1);
            }
        }
        let next = self.next.unwrap_or(next);
        self.calm += 1;
        if self.calm >= CALM_TICKS {
            self.calm = 0;
            self.depth = (self.depth - 1).max(MIN_DEPTH);
        }
        if let Some(payload) = self.packets.remove(&next) {
            self.next = Some(next + 1);
            self.concealed = 0;
            return Playout::Packet(payload);
        }
        if self.packets.is_empty() {
            self.concealed += 1;
            if self.concealed > MAX_CONCEAL {
                // The stream stopped: wait for it to fill the buffer again.
                self.next = None;
                self.concealed = 0;
                return Playout::Silence;
            }
            self.next = Some(next + 1);
            self.stats.lost += 1;
            return Playout::Conceal;
        }
        self.next = Some(next + 1);
        self.stats.lost += 1;
        self.concealed = 0;
        match self.packets.get(&(next + 1)) {
            Some(following) => Playout::Recover(following.clone()),
            None => Playout::Conceal,
        }
    }

    fn restart(&mut self) {
        self.packets.clear();
        self.base = None;
        self.next = None;
        self.concealed = 0;
    }

    /// Counts a 16-bit sequence number on from the first one, across the wrap.
    fn unwrap_sequence(&mut self, sequence: u16) -> i64 {
        let Some((last, counted)) = self.base else {
            self.base = Some((sequence, 0));
            return 0;
        };
        let step = i64::from(sequence.wrapping_sub(last) as i16);
        let counted = counted + step;
        if step > 0 {
            self.base = Some((sequence, counted));
        }
        counted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(n: u16) -> Vec<u8> {
        n.to_be_bytes().to_vec()
    }

    /// The datagrams of `count` packets, the payload of each its own number.
    fn stream(epoch: u8, count: u16) -> Vec<Vec<u8>> {
        let mut packetiser = AudioPacketiser::new(epoch);
        (0..count)
            .map(|n| packetiser.packet(&payload(n)).unwrap())
            .collect()
    }

    fn packet_number(playout: &Playout) -> Option<u16> {
        match playout {
            Playout::Packet(p) => Some(u16::from_be_bytes([p[0], p[1]])),
            _ => None,
        }
    }

    #[test]
    fn the_packetiser_counts_packets_and_samples() {
        let datagrams = stream(3, 3);
        let third = AudioDatagram::parse(&datagrams[2]).unwrap();
        assert_eq!((third.epoch, third.sequence, third.timestamp), (3, 2, 960));
        assert!(!third.interrupted);
        let mut packetiser = AudioPacketiser::new(0);
        packetiser.interrupt();
        let first = packetiser.packet(b"x").unwrap();
        let second = packetiser.packet(b"y").unwrap();
        assert!(AudioDatagram::parse(&first).unwrap().interrupted);
        assert!(!AudioDatagram::parse(&second).unwrap().interrupted);
    }

    #[test]
    fn plays_after_40_ms_in_order_and_complete() {
        let mut jitter = AudioJitter::new();
        let datagrams = stream(0, 20);
        let mut played = Vec::new();
        for (n, datagram) in datagrams.iter().enumerate() {
            jitter.insert(datagram).unwrap();
            let playout = jitter.pop();
            if n + 1 < MIN_DEPTH {
                assert_eq!(playout, Playout::Silence, "still filling at {n}");
            }
            played.extend(packet_number(&playout));
        }
        while played.len() < 20 {
            played.extend(packet_number(&jitter.pop()));
        }
        assert_eq!(played, (0..20).collect::<Vec<_>>());
        assert_eq!(jitter.stats().lost, 0);
    }

    #[test]
    fn puts_reordered_packets_back() {
        let mut jitter = AudioJitter::new();
        let datagrams = stream(0, 8);
        for i in [1, 0, 3, 2, 5, 4, 7, 6] {
            jitter.insert(&datagrams[i]).unwrap();
        }
        let played: Vec<_> = (0..8)
            .filter_map(|_| packet_number(&jitter.pop()))
            .collect();
        assert_eq!(played, (0..8).collect::<Vec<_>>());
    }

    #[test]
    fn a_lost_packet_is_rebuilt_from_the_next_one() {
        let mut jitter = AudioJitter::new();
        let datagrams = stream(0, 8);
        for (n, datagram) in datagrams.iter().enumerate() {
            if n != 4 {
                jitter.insert(datagram).unwrap();
            }
        }
        let playouts: Vec<_> = (0..8).map(|_| jitter.pop()).collect();
        assert_eq!(packet_number(&playouts[3]), Some(3));
        assert_eq!(playouts[4], Playout::Recover(payload(5)));
        assert_eq!(packet_number(&playouts[5]), Some(5));
        assert_eq!(jitter.stats().lost, 1);
    }

    #[test]
    fn a_late_packet_is_dropped_and_the_wait_grows() {
        let mut jitter = AudioJitter::new();
        let datagrams = stream(0, 10);
        for datagram in &datagrams[..4] {
            jitter.insert(datagram).unwrap();
        }
        // 0 to 3 play; 4 is missing when its time comes.
        for _ in 0..4 {
            assert!(packet_number(&jitter.pop()).is_some());
        }
        assert_eq!(jitter.pop(), Playout::Conceal);
        jitter.insert(&datagrams[4]).unwrap();
        assert_eq!(jitter.stats().late, 1);
        assert_eq!(jitter.stats().depth, MIN_DEPTH + 1);
    }

    #[test]
    fn the_wait_never_grows_past_120_ms() {
        let mut jitter = AudioJitter::new();
        let datagrams = stream(0, 40);
        for datagram in &datagrams[..4] {
            jitter.insert(datagram).unwrap();
        }
        for _ in 0..4 {
            jitter.pop();
        }
        // Every packet from here arrives just after its time.
        for datagram in &datagrams[4..14] {
            assert_eq!(jitter.pop(), Playout::Conceal);
            jitter.insert(datagram).unwrap();
        }
        assert_eq!(jitter.stats().late, 10);
        assert_eq!(jitter.stats().depth, MAX_DEPTH);
    }

    #[test]
    fn a_burst_after_a_stall_does_not_keep_the_delay() {
        let mut jitter = AudioJitter::new();
        for datagram in stream(0, 40) {
            jitter.insert(&datagram).unwrap();
        }
        let first = packet_number(&jitter.pop()).unwrap();
        assert!(
            usize::from(first) >= 40 - MIN_DEPTH - SLACK - 1,
            "played {first} of 40: the delay was kept"
        );
        assert!(jitter.stats().skipped > 0);
    }

    #[test]
    fn a_stream_that_stops_is_given_up_and_starts_again() {
        let mut jitter = AudioJitter::new();
        let datagrams = stream(0, 30);
        for datagram in &datagrams[..4] {
            jitter.insert(datagram).unwrap();
        }
        for _ in 0..4 {
            jitter.pop();
        }
        let conceal = (0..30).filter(|_| jitter.pop() == Playout::Conceal).count();
        assert_eq!(conceal, MAX_CONCEAL as usize);
        assert_eq!(jitter.pop(), Playout::Silence);
        for datagram in &datagrams[20..24] {
            jitter.insert(datagram).unwrap();
        }
        assert_eq!(packet_number(&jitter.pop()), Some(20));
    }

    #[test]
    fn a_new_epoch_or_an_interruption_starts_over() {
        let mut jitter = AudioJitter::new();
        for datagram in stream(0, 6) {
            jitter.insert(&datagram).unwrap();
        }
        assert_eq!(packet_number(&jitter.pop()), Some(0));
        // A new epoch numbers from 0 again; the old packets go.
        for datagram in stream(1, 4) {
            jitter.insert(&datagram).unwrap();
        }
        let played: Vec<_> = (0..4)
            .filter_map(|_| packet_number(&jitter.pop()))
            .collect();
        assert_eq!(played, vec![0, 1, 2, 3]);
    }

    #[test]
    fn follows_the_sequence_number_across_the_wrap() {
        let mut jitter = AudioJitter::new();
        let mut packetiser = AudioPacketiser::new(0);
        packetiser.sequence = u16::MAX - 2;
        let datagrams: Vec<_> = (0..8u16)
            .map(|n| packetiser.packet(&payload(n)).unwrap())
            .collect();
        for datagram in &datagrams {
            jitter.insert(datagram).unwrap();
        }
        let played: Vec<_> = (0..8)
            .filter_map(|_| packet_number(&jitter.pop()))
            .collect();
        assert_eq!(played, (0..8).collect::<Vec<_>>());
    }

    #[test]
    fn duplicates_and_bad_datagrams_change_nothing() {
        let mut jitter = AudioJitter::new();
        let datagrams = stream(0, 4);
        for datagram in &datagrams {
            jitter.insert(datagram).unwrap();
            jitter.insert(datagram).unwrap();
        }
        assert!(jitter.insert(b"\x00\x02").is_err());
        let played: Vec<_> = (0..6)
            .filter_map(|_| packet_number(&jitter.pop()))
            .collect();
        assert_eq!(played, vec![0, 1, 2, 3]);
    }
}
