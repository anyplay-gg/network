//! The game's sound on the wire (`spec/share-play.md`, audio datagram): the sender's numbering
//! of Opus packets, and the viewer's jitter buffer that puts them back in order and says, for
//! every 10 ms the speakers ask for, what the decoder should do. It holds Opus packets, not
//! sound, so it is tested without a codec.

use std::collections::{BTreeMap, BTreeSet};

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
/// Most packets held at once; a sender cannot make the buffer grow past this.
const MAX_HELD: usize = 512;
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
    /// The first packet says the stream starts, so a viewer that heard an earlier sender
    /// starts its buffer again.
    pub fn new(epoch: u8) -> AudioPacketiser {
        AudioPacketiser {
            epoch,
            sequence: 0,
            timestamp: 0,
            interrupted: true,
        }
    }

    /// The sound stopped for a while (nothing captured): the next packet says so, and the
    /// viewer starts its buffer again.
    pub fn interrupt(&mut self) {
        self.interrupted = true;
    }

    pub fn epoch(&self) -> u8 {
        self.epoch
    }

    /// The stream changed (`spec/share-play.md`: the epoch is as for video). The next packet
    /// carries the new epoch and says the stream was interrupted.
    pub fn set_epoch(&mut self, epoch: u8) {
        if epoch != self.epoch {
            self.epoch = epoch;
            self.interrupted = true;
        }
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
    /// No packet to play: let the codec fill in. Either the packet due is missing and nothing
    /// can rebuild it, or the buffer waits to grow deeper after a late packet.
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
    /// Ticks filled in while the buffer grew deeper.
    pub held: u64,
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
    /// The first packet after the latest gap: anything older is from before it.
    start: Option<i64>,
    /// A packet came late: play nothing new until the buffer is `depth` packets deep.
    hold: bool,
    /// Packets whose time passed without them, newest last: one of them arriving is late; any
    /// other packet from before `next` is a copy.
    missed: BTreeSet<i64>,
    concealed: u32,
    calm: u32,
    stats: AudioStats,
    /// What the latest `pop` dropped to catch up, oldest first.
    dropped: Vec<Vec<u8>>,
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
            start: None,
            hold: false,
            missed: BTreeSet::new(),
            concealed: 0,
            calm: 0,
            stats: AudioStats::default(),
            dropped: Vec::new(),
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
        let same_epoch = self.epoch == Some(datagram.epoch);
        let reordered = datagram.interrupted && same_epoch && self.is_old(datagram.sequence);
        if (datagram.interrupted && !reordered) || !same_epoch {
            self.restart();
            self.epoch = Some(datagram.epoch);
        }
        let sequence = self.unwrap_sequence(datagram.sequence);
        if datagram.interrupted && !reordered {
            self.start = Some(sequence);
        }
        if self.start.is_some_and(|start| sequence < start) {
            // From before the gap that started this stretch.
            return Ok(());
        }
        if reordered {
            // The start of this stretch, after packets that follow it: what came before it is
            // from before the gap.
            if self.start.is_some_and(|start| sequence <= start) {
                return Ok(());
            }
            self.start = Some(sequence);
            self.packets.retain(|&s, _| s >= sequence);
            self.missed.retain(|&s| s >= sequence);
        }
        self.stats.received += 1;
        if self.next.is_some_and(|next| sequence < next) {
            if self.missed.remove(&sequence) {
                // Its time has passed: wait a packet longer from now on.
                self.stats.late += 1;
                self.depth = (self.depth + 1).min(MAX_DEPTH);
                self.hold = true;
                self.calm = 0;
            }
            return Ok(());
        }
        self.packets
            .entry(sequence)
            .or_insert_with(|| datagram.payload.to_vec());
        while self.packets.len() > MAX_HELD {
            self.packets.pop_first();
        }
        Ok(())
    }

    /// What to play for the next 10 ms.
    pub fn pop(&mut self) -> Playout {
        self.dropped.clear();
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
        // Too far behind the newest packet: drop the oldest, back to the depth.
        if self.packets.len() > self.depth + SLACK {
            while self.packets.len() > self.depth {
                if let Some((sequence, payload)) = self.packets.pop_first() {
                    self.dropped.push(payload);
                    self.stats.skipped += 1;
                    self.next = Some(sequence + 1);
                }
            }
            self.hold = false;
        }
        let next = self.next.unwrap_or(next);
        self.calm += 1;
        if self.calm >= CALM_TICKS {
            self.calm = 0;
            self.depth = (self.depth - 1).max(MIN_DEPTH);
        }
        if self.hold && self.packets.len() < self.depth {
            // Growing deeper: fill in without moving on.
            return self.conceal_or_give_up(|jitter| jitter.stats.held += 1);
        }
        self.hold = false;
        if let Some(payload) = self.packets.remove(&next) {
            self.next = Some(next + 1);
            self.concealed = 0;
            return Playout::Packet(payload);
        }
        self.miss(next);
        if self.packets.is_empty() {
            return self.conceal_or_give_up(|_| {});
        }
        self.concealed = 0;
        match self.packets.get(&(next + 1)) {
            Some(following) => Playout::Recover(following.clone()),
            None => Playout::Conceal,
        }
    }

    /// The packets the latest [`pop`](Self::pop) dropped to catch up, oldest first. They go
    /// through the decoder before what `pop` answered, unheard: a decoder that never saw them
    /// joins two stretches of sound that were not next to each other, and the join overshoots
    /// (a click, up to half again as loud as the sound).
    pub fn take_dropped(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.dropped)
    }

    /// The packet due is not here: it is counted lost, and remembered in case it comes late.
    fn miss(&mut self, sequence: i64) {
        self.next = Some(sequence + 1);
        self.stats.lost += 1;
        self.missed.insert(sequence);
        while self.missed.len() > 2 * MAX_DEPTH {
            self.missed.pop_first();
        }
    }

    /// Conceals, unless nothing came for too long: then the stream stopped, and the buffer
    /// waits for it to fill again.
    fn conceal_or_give_up(&mut self, count: impl FnOnce(&mut Self)) -> Playout {
        self.concealed += 1;
        if self.concealed > MAX_CONCEAL {
            self.next = None;
            self.hold = false;
            self.concealed = 0;
            return Playout::Silence;
        }
        count(self);
        Playout::Conceal
    }

    fn restart(&mut self) {
        self.packets.clear();
        self.base = None;
        self.start = None;
        self.next = None;
        self.hold = false;
        self.missed.clear();
        self.concealed = 0;
    }

    /// Whether `sequence` comes before the newest packet seen of this stretch.
    fn is_old(&self, sequence: u16) -> bool {
        self.base
            .is_some_and(|(last, _)| (sequence.wrapping_sub(last) as i16) <= 0)
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
    fn a_sender_cannot_make_the_buffer_grow_without_end() {
        let mut jitter = AudioJitter::new();
        for datagram in stream(3, 3000) {
            let _ = jitter.insert(&datagram);
        }
        assert!(jitter.packets.len() <= MAX_HELD);
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

    /// Plays `ticks` ticks of 10 ms; packet `n` of `count` arrives at tick `arrive(n)`, before
    /// that tick's play. Answers the buffer and, per packet played, its number and the ticks it
    /// waited between arriving and playing.
    fn waits(
        count: u16,
        ticks: usize,
        arrive: impl Fn(u16) -> usize,
    ) -> (AudioJitter, Vec<(u16, usize)>) {
        let datagrams = stream(0, count);
        let mut jitter = AudioJitter::new();
        let mut played = Vec::new();
        for tick in 0..ticks {
            for n in (0..count).filter(|&n| arrive(n) == tick) {
                jitter.insert(&datagrams[usize::from(n)]).unwrap();
            }
            if let Some(n) = packet_number(&jitter.pop()) {
                played.push((n, tick - arrive(n)));
            }
        }
        (jitter, played)
    }

    #[test]
    fn a_stall_makes_the_wait_longer_so_the_next_one_loses_nothing() {
        // Two stalls of 60 ms: packets 20 to 25 come with 26, and 60 to 65 with 66.
        let stalled = |n: u16| match n {
            20..=25 => 26,
            60..=65 => 66,
            n => usize::from(n),
        };
        let (jitter, played) = waits(100, 120, stalled);
        let numbers: Vec<u16> = played.iter().map(|&(n, _)| n).collect();
        // The first stall loses the packets whose time passed; the second loses none.
        assert!(!numbers.contains(&20), "20 came 60 ms late");
        assert!((60..66).all(|n| numbers.contains(&n)), "{numbers:?}");
        let after_first = played.iter().find(|&&(n, _)| n == 40).unwrap().1;
        assert!(
            after_first >= 6,
            "waited {after_first} ticks after a 60 ms stall"
        );
        assert_eq!(jitter.stats().late, 3);
    }

    #[test]
    fn a_slow_sender_does_not_drain_the_buffer_into_losing_every_jitter() {
        // The sender's clock is 1% slow: its packets come 1% further apart than they play.
        // Until the last packet arrives, at tick 2018; the ones still queued then are not lost.
        let (jitter, _) = waits(2000, 2019, |n| usize::from(n) * 101 / 100);
        // Every time the buffer runs dry it fills to its depth again, so running dry once
        // costs one packet and the next time is depth x 100 packets later; staying dry would
        // cost one in every 100.
        let lost = jitter.stats().lost;
        assert!(
            lost <= 5,
            "{lost} packets lost in 20 s: {:?}",
            jitter.stats()
        );
    }

    #[test]
    fn the_wait_never_grows_past_120_ms_whatever_the_stalls() {
        // Every 50 packets a stall: the first of 100 ms, the others of 150 ms, the packets of
        // which come together at its end.
        let stalled = |n: u16| {
            let n = usize::from(n);
            let stall = if n < 50 { 10 } else { 15 };
            let (block, at) = (n - n % 50, n % 50);
            if (30..30 + stall).contains(&at) {
                block + 30 + stall
            } else {
                n
            }
        };
        let (jitter, played) = waits(1000, 1050, stalled);
        assert_eq!(jitter.stats().depth, MAX_DEPTH);
        let longest = played.iter().map(|&(_, w)| w).max().unwrap();
        assert!(longest <= MAX_DEPTH, "a packet waited {longest} ticks");
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
        // 5 to 8 are here but play only once the buffer holds 5.
        for datagram in &datagrams[5..9] {
            jitter.insert(datagram).unwrap();
        }
        assert_eq!(jitter.pop(), Playout::Conceal);
        jitter.insert(&datagrams[9]).unwrap();
        assert_eq!(packet_number(&jitter.pop()), Some(5));
        assert_eq!(jitter.stats().held, 1);
    }

    #[test]
    fn a_burst_after_a_stall_does_not_keep_the_delay() {
        let mut jitter = AudioJitter::new();
        for datagram in stream(0, 40) {
            jitter.insert(&datagram).unwrap();
        }
        let first = packet_number(&jitter.pop()).unwrap();
        assert!(
            usize::from(first) >= 40 - MIN_DEPTH,
            "played {first} of 40: the delay was kept"
        );
        assert!(jitter.stats().skipped > 0);
        let dropped = jitter.take_dropped();
        assert_eq!(dropped.len() as u64, jitter.stats().skipped);
        assert_eq!(dropped, (0..first).map(payload).collect::<Vec<_>>());
        assert!(jitter.take_dropped().is_empty());
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
        // A copy of a packet that already played is not a late one.
        jitter.insert(&datagrams[0]).unwrap();
        assert_eq!(jitter.stats().late, 0);
        assert_eq!(jitter.stats().depth, MIN_DEPTH);
    }

    #[test]
    fn an_interruption_starts_over_and_an_old_one_changes_nothing() {
        let mut jitter = AudioJitter::new();
        let mut packetiser = AudioPacketiser::new(0);
        let before: Vec<_> = (0..6)
            .map(|n| packetiser.packet(&payload(n)).unwrap())
            .collect();
        packetiser.interrupt();
        let after: Vec<_> = (100..106)
            .map(|n| packetiser.packet(&payload(n)).unwrap())
            .collect();
        for datagram in &before {
            jitter.insert(datagram).unwrap();
        }
        assert_eq!(packet_number(&jitter.pop()), Some(0));
        for datagram in after.iter().take(4) {
            jitter.insert(datagram).unwrap();
        }
        // The packets from before the gap are gone.
        assert_eq!(packet_number(&jitter.pop()), Some(100));
        // The first packet of a stream is marked interrupted; a copy arriving late does not
        // wipe what came after it.
        jitter.insert(&before[0]).unwrap();
        jitter.insert(&after[0]).unwrap();
        assert_eq!(packet_number(&jitter.pop()), Some(101));
    }

    #[test]
    fn a_new_sender_says_it_starts_a_stream() {
        let first = AudioPacketiser::new(0).packet(b"x").unwrap();
        assert!(AudioDatagram::parse(&first).unwrap().interrupted);
    }
}
