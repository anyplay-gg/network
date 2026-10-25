//! Puts video datagrams back together: recovers lost packets from parity, hands out complete
//! frames in order, and never hands out a damaged one.
//!
//! What "never damaged" means here: a frame is delivered only when every packet of it was
//! received or rebuilt and its length checks out, and a delta frame is delivered only when the
//! frame right before it was delivered too. After any frame is lost the reassembler asks for a
//! key frame ([`Reassembler::poll_key_request`]) and delivers nothing but a key frame until one
//! arrives, so the decoder never sees a picture built on a missing reference.
//!
//! Time comes in as microseconds from any monotonic clock the caller owns.

use std::collections::VecDeque;

use crate::codec::EncodedFrame;
use crate::packetiser::LENGTH_PREFIX;
use crate::rs;
use crate::wire::{Refusal, VideoDatagram};

/// Frames kept in reassembly at once.
pub const MAX_FRAMES: usize = 4;
/// Payload bytes kept in reassembly at once.
pub const MAX_BYTES: usize = 16 * 1024 * 1024;

const TOMBSTONES: usize = 16;
const TOMBSTONE_LIFE_US: u64 = 100_000;

/// `a` comes after `b`, counting frame numbers modulo 2^32.
fn newer(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// Tunable limits. The defaults are the spec's where it gives numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub max_frames: usize,
    pub max_bytes: usize,
    /// How long a complete frame waits for an earlier frame that is still missing packets (or
    /// has not shown up at all) before the earlier one is given up.
    pub reorder_wait_us: u64,
    /// An incomplete frame is given up this long after its first packet.
    pub frame_timeout_us: u64,
    /// At most one key frame request per this long.
    pub key_request_interval_us: u64,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            max_frames: MAX_FRAMES,
            max_bytes: MAX_BYTES,
            reorder_wait_us: 20_000,
            frame_timeout_us: 100_000,
            key_request_interval_us: 500_000,
        }
    }
}

/// What became of a datagram that was well formed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Kept for its frame.
    Stored,
    /// The same packet came twice.
    Duplicate,
    /// From another epoch than the current one.
    WrongEpoch,
    /// For a frame that is already delivered or given up.
    Late,
    /// Contradicts the other packets of its frame (key flag, timestamp, group layout).
    Inconsistent,
    /// Its frame was dropped to stay inside the memory limits.
    Dropped,
}

/// A request for a key frame: the control message `key`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyRequest {
    pub epoch: u8,
    /// A key frame is needed from this frame number on.
    pub frame: u32,
}

/// A complete frame, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reassembled {
    /// The frame number the sender gave it.
    pub frame: u32,
    pub encoded: EncodedFrame,
}

/// Counters since the start (all datagram counts are packets, data and parity).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Packets accepted into a frame, including late ones for a closed frame.
    pub received: u64,
    /// Packets of closed frames that never came.
    pub lost: u64,
    /// Data packets rebuilt from parity in frames that were delivered.
    pub recovered: u64,
    pub frames_delivered: u64,
    /// Frames given up because packets were missing.
    pub frames_dropped: u64,
    /// Complete frames thrown away because a frame before them was lost and no key frame has
    /// come yet.
    pub frames_discarded: u64,
    /// Frames whose length or layout did not check out.
    pub frames_damaged: u64,
    pub duplicates: u64,
    pub late: u64,
    pub refused: u64,
    pub wrong_epoch: u64,
}

/// What the `report` message carries about the packets, since the last
/// [`Reassembler::take_interval`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Interval {
    pub received: u32,
    pub lost: u32,
    pub recovered: u32,
}

struct Group {
    data: u16,
    parity: u16,
    shards: Vec<Option<Vec<u8>>>,
    have: u16,
}

struct Asm {
    frame: u32,
    key: bool,
    timestamp: u32,
    groups: Vec<Option<Group>>,
    /// The length of a full packet, once a packet shows it.
    len: Option<usize>,
    first_us: u64,
    bytes: usize,
    received: u32,
}

impl Asm {
    fn complete(&self) -> bool {
        self.groups
            .iter()
            .all(|g| g.as_ref().is_some_and(|g| g.have >= g.data))
    }

    /// Packets the frame has in all, estimated from the groups seen so far.
    fn expected(&self) -> u32 {
        let (mut sum, mut seen) = (0u32, 0u32);
        for g in self.groups.iter().flatten() {
            sum += u32::from(g.data) + u32::from(g.parity);
            seen += 1;
        }
        (sum * self.groups.len() as u32)
            .checked_div(seen)
            .unwrap_or(0)
    }

    /// Rebuild the encoded frame. `None` when what is there does not add up. Also returns the
    /// number of data packets rebuilt from parity.
    fn reassemble(&mut self) -> Option<(Vec<u8>, u32)> {
        let last_group = self.groups.len() - 1;
        let mut l = self.len;
        if l.is_none() {
            // Only the last packet of a one-packet frame can be all there is.
            let g = self.groups[last_group].as_ref()?;
            l = Some(g.shards[usize::from(g.data) - 1].as_ref()?.len());
        }
        let l = l?;
        let mut stream = Vec::new();
        let mut packets = 0usize;
        let mut last_actual = None;
        let mut recovered = 0u32;
        for (gi, g) in self.groups.iter_mut().enumerate() {
            let g = g.as_mut()?;
            let (d, p) = (usize::from(g.data), usize::from(g.parity));
            for (i, s) in g.shards.iter_mut().enumerate() {
                if let Some(v) = s {
                    if v.len() > l {
                        return None;
                    }
                    if gi == last_group && i + 1 == d {
                        last_actual = Some(v.len());
                    } else if v.len() != l {
                        return None;
                    }
                    v.resize(l, 0);
                }
            }
            let missing = g.shards[..d].iter().filter(|s| s.is_none()).count();
            if missing > 0 {
                if !rs::reconstruct(d, p, &mut g.shards, l) {
                    return None;
                }
                recovered += missing as u32;
            }
            for s in &g.shards[..d] {
                stream.extend_from_slice(s.as_ref()?);
            }
            packets += d;
        }
        let size = match last_actual {
            Some(a) => (packets - 1) * l + a,
            None => packets * l,
        };
        stream.truncate(size);
        if stream.len() < LENGTH_PREFIX {
            return None;
        }
        let flen = u32::from_be_bytes([stream[0], stream[1], stream[2], stream[3]]) as usize;
        let framed = flen.checked_add(LENGTH_PREFIX)?;
        let fits = match last_actual {
            Some(_) => framed == size,
            None => framed > (packets - 1) * l && framed <= packets * l,
        };
        if !fits {
            return None;
        }
        stream.drain(..LENGTH_PREFIX);
        stream.truncate(flen);
        Some((stream, recovered))
    }
}

struct Tombstone {
    frame: u32,
    expected: u32,
    received: u32,
    first_us: u64,
}

/// The receiving half. See the module documentation.
pub struct Reassembler {
    cfg: Config,
    epoch: u8,
    /// Sorted by frame number, oldest first.
    frames: Vec<Asm>,
    bytes: usize,
    tombstones: VecDeque<Tombstone>,
    newest_closed: Option<u32>,
    newest_delivered: Option<u32>,
    newest_seen: Option<u32>,
    /// Only a key frame may be delivered next.
    need_key: bool,
    /// A key frame was asked for and has not come.
    key_wanted: bool,
    key_wanted_since: Option<u64>,
    last_key_request: Option<u64>,
    stats: Stats,
    interval_base: Stats,
}

impl Reassembler {
    pub fn new(epoch: u8) -> Reassembler {
        Reassembler::with_config(epoch, Config::default())
    }

    pub fn with_config(epoch: u8, cfg: Config) -> Reassembler {
        Reassembler {
            cfg,
            epoch,
            frames: Vec::new(),
            bytes: 0,
            tombstones: VecDeque::new(),
            newest_closed: None,
            newest_delivered: None,
            newest_seen: None,
            need_key: true,
            key_wanted: false,
            key_wanted_since: None,
            last_key_request: None,
            stats: Stats::default(),
            interval_base: Stats::default(),
        }
    }

    pub fn epoch(&self) -> u8 {
        self.epoch
    }

    /// A new stream setup: everything in reassembly is dropped and a key frame is awaited.
    pub fn set_epoch(&mut self, epoch: u8) {
        self.epoch = epoch;
        self.frames.clear();
        self.bytes = 0;
        self.tombstones.clear();
        self.newest_closed = None;
        self.newest_delivered = None;
        self.newest_seen = None;
        self.need_key = true;
        self.key_wanted = false;
        self.key_wanted_since = None;
        self.last_key_request = None;
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Packet counts since the last call, for the `report` message.
    pub fn take_interval(&mut self) -> Interval {
        let now = self.stats;
        let before = std::mem::replace(&mut self.interval_base, now);
        let d = |a: u64, b: u64| u32::try_from(a - b).unwrap_or(u32::MAX);
        Interval {
            received: d(now.received, before.received),
            lost: d(now.lost, before.lost),
            recovered: d(now.recovered, before.recovered),
        }
    }

    /// The highest frame number any accepted packet carried.
    pub fn newest_frame(&self) -> Option<u32> {
        self.newest_seen
    }

    /// Since when a key frame has been wanted and has not come. The viewer shows that the
    /// connection is poor when this is over 2 seconds.
    pub fn awaiting_key_since(&self) -> Option<u64> {
        self.key_wanted_since
    }

    /// Frames in reassembly right now, and their payload bytes.
    pub fn in_flight(&self) -> (usize, usize) {
        (self.frames.len(), self.bytes)
    }

    /// Whether a key frame should be asked for now: at most once per
    /// [`Config::key_request_interval_us`], and again after that for as long as none came.
    pub fn poll_key_request(&mut self, now_us: u64) -> Option<KeyRequest> {
        if !self.key_wanted {
            return None;
        }
        if let Some(t) = self.last_key_request
            && now_us < t.saturating_add(self.cfg.key_request_interval_us)
        {
            return None;
        }
        self.last_key_request = Some(now_us);
        Some(KeyRequest {
            epoch: self.epoch,
            frame: self.newest_seen.map_or(0, |f| f.wrapping_add(1)),
        })
    }

    /// Take in one datagram. Hand out the frames it completes with
    /// [`poll_frame`](Self::poll_frame).
    pub fn on_datagram(&mut self, now_us: u64, bytes: &[u8]) -> Result<Outcome, Refusal> {
        let dg = match VideoDatagram::parse(bytes) {
            Ok(d) => d,
            Err(e) => {
                self.stats.refused += 1;
                return Err(e);
            }
        };
        if dg.epoch != self.epoch {
            self.stats.wrong_epoch += 1;
            return Ok(Outcome::WrongEpoch);
        }
        self.expire(now_us);
        let r = self.insert(now_us, &dg);
        match &r {
            Err(_) => self.stats.refused += 1,
            Ok(Outcome::Duplicate) => self.stats.duplicates += 1,
            Ok(Outcome::Late) => self.stats.late += 1,
            Ok(_) => {}
        }
        r
    }

    fn insert(&mut self, now_us: u64, dg: &VideoDatagram<'_>) -> Result<Outcome, Refusal> {
        let pos = match self.frames.iter().position(|a| a.frame == dg.frame) {
            Some(p) => p,
            None => {
                if let Some(nc) = self.newest_closed
                    && !newer(dg.frame, nc)
                {
                    // A straggler of a frame already closed still counts as arrived.
                    if let Some(t) = self.tombstones.iter_mut().find(|t| t.frame == dg.frame) {
                        t.received += 1;
                        self.stats.received += 1;
                    }
                    return Ok(Outcome::Late);
                }
                if self.frames.len() >= self.cfg.max_frames.max(1) {
                    if !newer(dg.frame, self.frames[0].frame) {
                        return Ok(Outcome::Late);
                    }
                    let oldest = self.frames.remove(0);
                    self.close(oldest, now_us);
                    self.stats.frames_dropped += 1;
                    self.want_key(now_us);
                }
                let at = self
                    .frames
                    .iter()
                    .position(|a| newer(a.frame, dg.frame))
                    .unwrap_or(self.frames.len());
                self.frames.insert(
                    at,
                    Asm {
                        frame: dg.frame,
                        key: dg.key,
                        timestamp: dg.timestamp,
                        groups: (0..dg.groups).map(|_| None).collect(),
                        len: None,
                        first_us: now_us,
                        bytes: 0,
                        received: 0,
                    },
                );
                at
            }
        };

        let n = dg.payload.len();
        let asm = &mut self.frames[pos];
        if asm.key != dg.key
            || asm.timestamp != dg.timestamp
            || asm.groups.len() != dg.groups as usize
        {
            return Ok(Outcome::Inconsistent);
        }
        let group = &mut asm.groups[usize::from(dg.group)];
        let g = group.get_or_insert_with(|| Group {
            data: dg.data,
            parity: dg.parity,
            shards: vec![None; usize::from(dg.data) + usize::from(dg.parity)],
            have: 0,
        });
        if g.data != dg.data || g.parity != dg.parity {
            return Ok(Outcome::Inconsistent);
        }
        if g.shards[usize::from(dg.index)].is_some() {
            return Ok(Outcome::Duplicate);
        }

        // Every packet of a frame is as long as the first one, except the last data packet of
        // the last group, which may be shorter.
        let is_last = usize::from(dg.group) + 1 == asm.groups.len()
            && usize::from(dg.index) + 1 == usize::from(dg.data);
        if is_last {
            if asm.len.is_some_and(|l| n > l) {
                return Err(Refusal::PayloadLength);
            }
        } else {
            match asm.len {
                Some(l) if l != n => return Err(Refusal::PayloadLength),
                Some(_) => {}
                None => {
                    // The last packet, if already here, must not be longer than a full one.
                    let last_len = asm
                        .groups
                        .last()
                        .and_then(|g| g.as_ref())
                        .and_then(|g| g.shards[usize::from(g.data) - 1].as_ref())
                        .map(Vec::len);
                    if last_len.is_some_and(|ll| ll > n) {
                        return Err(Refusal::PayloadLength);
                    }
                    asm.len = Some(n);
                }
            }
        }
        // Re-borrow: the group again, now that the frame-level checks are done.
        let g = asm.groups[usize::from(dg.group)]
            .as_mut()
            .expect("group was created above");
        g.shards[usize::from(dg.index)] = Some(dg.payload.to_vec());
        g.have += 1;
        asm.bytes += n;
        asm.received += 1;
        self.bytes += n;
        self.stats.received += 1;
        self.newest_seen = Some(match self.newest_seen {
            Some(s) if !newer(dg.frame, s) => s,
            _ => dg.frame,
        });

        // Stay inside the memory limit: the oldest frames go first.
        let mut dropped_self = false;
        while self.bytes > self.cfg.max_bytes && !self.frames.is_empty() {
            let oldest = self.frames.remove(0);
            dropped_self |= oldest.frame == dg.frame;
            self.close(oldest, now_us);
            self.stats.frames_dropped += 1;
            self.want_key(now_us);
        }
        Ok(if dropped_self {
            Outcome::Dropped
        } else {
            Outcome::Stored
        })
    }

    /// Take the next complete frame, if there is one that may be delivered now.
    pub fn poll_frame(&mut self, now_us: u64) -> Option<Reassembled> {
        self.expire(now_us);
        // Frames that stayed incomplete for too long are given up.
        let mut i = 0;
        while i < self.frames.len() {
            let a = &self.frames[i];
            if !a.complete() && now_us >= a.first_us.saturating_add(self.cfg.frame_timeout_us) {
                let a = self.frames.remove(i);
                self.close(a, now_us);
                self.stats.frames_dropped += 1;
                self.want_key(now_us);
            } else {
                i += 1;
            }
        }
        loop {
            if self.frames.is_empty() {
                return None;
            }
            if !self.frames[0].complete() {
                let k = self.frames.iter().position(Asm::complete)?;
                if self.frames[k].key {
                    // A key frame stands for everything before it.
                    for _ in 0..k {
                        let a = self.frames.remove(0);
                        self.close(a, now_us);
                        self.stats.frames_dropped += 1;
                    }
                    continue;
                }
                if now_us
                    >= self.frames[0]
                        .first_us
                        .saturating_add(self.cfg.reorder_wait_us)
                {
                    let a = self.frames.remove(0);
                    self.close(a, now_us);
                    self.stats.frames_dropped += 1;
                    self.want_key(now_us);
                    continue;
                }
                return None;
            }
            let (key, number) = (self.frames[0].key, self.frames[0].frame);
            if !key {
                if self.need_key {
                    let a = self.frames.remove(0);
                    self.close(a, now_us);
                    self.stats.frames_discarded += 1;
                    self.want_key(now_us);
                    continue;
                }
                if self.newest_delivered.map(|n| n.wrapping_add(1)) != Some(number) {
                    // A frame before this one is missing altogether. It may still be on its way.
                    if now_us
                        < self.frames[0]
                            .first_us
                            .saturating_add(self.cfg.reorder_wait_us)
                    {
                        return None;
                    }
                    let a = self.frames.remove(0);
                    self.close(a, now_us);
                    self.stats.frames_discarded += 1;
                    self.want_key(now_us);
                    continue;
                }
            }
            let mut a = self.frames.remove(0);
            match a.reassemble() {
                None => {
                    self.close(a, now_us);
                    self.stats.frames_damaged += 1;
                    self.want_key(now_us);
                }
                Some((data, recovered)) => {
                    if key {
                        self.need_key = false;
                        self.key_wanted = false;
                        self.key_wanted_since = None;
                    }
                    self.newest_delivered = Some(number);
                    self.stats.recovered += u64::from(recovered);
                    self.stats.frames_delivered += 1;
                    let out = Reassembled {
                        frame: number,
                        encoded: EncodedFrame {
                            key,
                            timestamp_us: a.timestamp,
                            data,
                        },
                    };
                    self.close(a, now_us);
                    return Some(out);
                }
            }
        }
    }

    fn want_key(&mut self, now_us: u64) {
        self.need_key = true;
        if !self.key_wanted {
            self.key_wanted = true;
            self.key_wanted_since = Some(now_us);
        }
    }

    /// A frame leaves reassembly; remember its packet counts until the stragglers are in.
    fn close(&mut self, a: Asm, now_us: u64) {
        self.bytes = self.bytes.saturating_sub(a.bytes);
        self.newest_closed = Some(match self.newest_closed {
            Some(c) if !newer(a.frame, c) => c,
            _ => a.frame,
        });
        let _ = now_us;
        self.tombstones.push_back(Tombstone {
            frame: a.frame,
            expected: a.expected(),
            received: a.received,
            first_us: a.first_us,
        });
        while self.tombstones.len() > TOMBSTONES {
            self.settle_front();
        }
    }

    fn settle_front(&mut self) {
        if let Some(t) = self.tombstones.pop_front() {
            self.stats.lost += u64::from(t.expected.saturating_sub(t.received));
        }
    }

    /// Count the packets of closed frames that never came, once waiting is pointless.
    fn expire(&mut self, now_us: u64) {
        while self
            .tombstones
            .front()
            .is_some_and(|t| now_us >= t.first_us.saturating_add(TOMBSTONE_LIFE_US))
        {
            self.settle_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packetiser::Packetiser;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    fn frame(len: usize, key: bool, seed: u8) -> EncodedFrame {
        EncodedFrame {
            key,
            timestamp_us: 1000 + u32::from(seed),
            data: (0..len)
                .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
                .collect(),
        }
    }

    fn feed(r: &mut Reassembler, now: u64, dgs: &[Vec<u8>]) {
        for d in dgs {
            r.on_datagram(now, d).unwrap();
        }
    }

    #[test]
    fn delivers_a_frame_identically() {
        for len in [0, 1, 995, 996, 997, 1000, 5000, 123_457] {
            let mut p = Packetiser::new(1000, 0);
            let mut r = Reassembler::new(0);
            let f = frame(len, true, 1);
            feed(&mut r, 10, &p.packetise(&f, 10).unwrap());
            let got = r.poll_frame(10).expect("complete");
            assert_eq!(got.encoded, f, "len {len}");
            assert_eq!(got.frame, 0);
            assert!(r.poll_frame(10).is_none());
        }
    }

    #[test]
    fn a_missing_last_short_packet_is_rebuilt_to_its_exact_length() {
        for len in [1, 700, 995, 996, 1500, 4000] {
            let mut p = Packetiser::new(1000, 0);
            let mut r = Reassembler::new(0);
            let f = frame(len, true, 2);
            let mut dgs = p.packetise(&f, 30).unwrap();
            let n_data = VideoDatagram::parse(&dgs[0]).unwrap().data as usize;
            dgs.remove(n_data - 1); // the last data packet
            feed(&mut r, 0, &dgs);
            let got = r.poll_frame(0).expect("rebuilt");
            assert_eq!(got.encoded.data, f.data, "len {len}");
            assert_eq!(r.stats().recovered, 1);
        }
    }

    #[test]
    fn recovers_up_to_the_parity_count_in_every_position() {
        let mut rng = Rng(77);
        for round in 0..200 {
            let mut p = Packetiser::new(1000, 0);
            let mut r = Reassembler::new(0);
            let len = 3000 + (rng.next() % 30_000) as usize;
            let f = frame(len, true, round as u8);
            let dgs = p.packetise(&f, 25).unwrap();
            let parity = VideoDatagram::parse(&dgs[0]).unwrap().parity as usize;
            let groups = VideoDatagram::parse(&dgs[0]).unwrap().groups;
            assert_eq!(groups, 1);
            // lose exactly `parity` packets
            let mut keep: Vec<_> = dgs.clone();
            for _ in 0..parity {
                let at = (rng.next() % keep.len() as u64) as usize;
                keep.remove(at);
            }
            feed(&mut r, 0, &keep);
            let got = r.poll_frame(0).expect("recoverable");
            assert_eq!(got.encoded.data, f.data);
        }
    }

    #[test]
    fn more_loss_than_parity_gives_up_and_asks_for_a_key_frame() {
        let mut p = Packetiser::new(1000, 0);
        let mut r = Reassembler::new(0);
        let k = frame(10_000, true, 1);
        feed(&mut r, 0, &p.packetise(&k, 10).unwrap());
        assert!(r.poll_frame(0).is_some());
        assert!(r.poll_key_request(0).is_none());

        let d1 = frame(10_000, false, 2);
        let mut dgs = p.packetise(&d1, 10).unwrap();
        dgs.drain(..4); // 4 gone, 1 parity
        feed(&mut r, 16_000, &dgs);
        assert!(r.poll_frame(16_000).is_none(), "still waiting");
        assert!(r.poll_frame(16_000 + 100_000).is_none());
        let req = r.poll_key_request(116_000).expect("asks");
        assert_eq!(req.frame, 2);
        assert_eq!(req.epoch, 0);
        assert_eq!(r.stats().frames_dropped, 1);
        // at most once per 500 ms
        assert!(r.poll_key_request(116_001).is_none());
        assert!(r.poll_key_request(116_000 + 499_999).is_none());
        assert!(r.poll_key_request(116_000 + 500_000).is_some());

        // a delta frame is not delivered while the key frame is missing
        let d2 = frame(2000, false, 3);
        feed(&mut r, 120_000, &p.packetise(&d2, 10).unwrap());
        assert!(r.poll_frame(120_000).is_none());
        assert_eq!(r.stats().frames_discarded, 1);
        // the key frame ends it
        let k2 = frame(4000, true, 4);
        feed(&mut r, 130_000, &p.packetise(&k2, 10).unwrap());
        let got = r.poll_frame(130_000).unwrap();
        assert_eq!(got.encoded, k2);
        assert!(r.poll_key_request(10_000_000).is_none());
        assert_eq!(r.awaiting_key_since(), None);
    }

    #[test]
    fn a_stream_that_starts_with_a_delta_frame_waits_for_a_key_frame() {
        let mut p = Packetiser::new(1000, 0);
        let mut r = Reassembler::new(0);
        feed(&mut r, 0, &p.packetise(&frame(500, false, 1), 0).unwrap());
        assert!(r.poll_frame(0).is_none());
        assert!(r.poll_key_request(0).is_some());
    }

    #[test]
    fn frames_come_out_in_order_and_a_late_one_is_dropped() {
        let mut p = Packetiser::new(1000, 0);
        let mut r = Reassembler::new(0);
        let a = p.packetise(&frame(3000, true, 1), 0).unwrap();
        let b = p.packetise(&frame(3000, false, 2), 0).unwrap();
        let c = p.packetise(&frame(3000, false, 3), 0).unwrap();
        // b arrives complete before a is complete
        feed(&mut r, 0, &a[..2]);
        feed(&mut r, 1000, &b);
        assert!(r.poll_frame(1000).is_none(), "b waits for a");
        feed(&mut r, 2000, &a[2..]);
        assert_eq!(r.poll_frame(2000).unwrap().frame, 0);
        assert_eq!(r.poll_frame(2000).unwrap().frame, 1);
        feed(&mut r, 3000, &c);
        assert_eq!(r.poll_frame(3000).unwrap().frame, 2);
        // a straggler of frame 1 is late now
        assert_eq!(r.on_datagram(4000, &b[0]), Ok(Outcome::Late));
        assert!(r.poll_frame(4000).is_none());
    }

    #[test]
    fn a_key_frame_supersedes_older_incomplete_frames() {
        let mut p = Packetiser::new(1000, 0);
        let mut r = Reassembler::new(0);
        feed(&mut r, 0, &p.packetise(&frame(3000, true, 1), 0).unwrap());
        assert!(r.poll_frame(0).is_some());
        let lost = p.packetise(&frame(3000, false, 2), 0).unwrap();
        feed(&mut r, 100, &lost[..2]);
        let key = p.packetise(&frame(3000, true, 3), 0).unwrap();
        feed(&mut r, 200, &key);
        let got = r.poll_frame(200).unwrap();
        assert_eq!(got.frame, 2);
        assert!(got.encoded.key);
    }

    #[test]
    fn a_gap_in_frame_numbers_discards_the_next_delta_frame() {
        let mut p = Packetiser::new(1000, 0);
        let mut r = Reassembler::new(0);
        feed(&mut r, 0, &p.packetise(&frame(500, true, 1), 0).unwrap());
        assert!(r.poll_frame(0).is_some());
        let _never_arrives = p.packetise(&frame(500, false, 2), 0).unwrap();
        feed(
            &mut r,
            1000,
            &p.packetise(&frame(500, false, 3), 0).unwrap(),
        );
        assert!(r.poll_frame(1000).is_none(), "waits a little for frame 1");
        assert!(r.poll_frame(1000 + 20_000).is_none());
        assert_eq!(r.stats().frames_discarded, 1);
        assert!(r.poll_key_request(30_000).is_some());
    }

    #[test]
    fn wrong_epoch_and_garbage_are_dropped() {
        let mut p = Packetiser::new(1000, 5);
        let mut r = Reassembler::new(6);
        let d = p.packetise(&frame(500, true, 1), 0).unwrap();
        assert_eq!(r.on_datagram(0, &d[0]), Ok(Outcome::WrongEpoch));
        assert_eq!(r.on_datagram(0, &[1, 2, 3]), Err(Refusal::TooShort));
        assert_eq!(r.stats().wrong_epoch, 1);
        assert_eq!(r.stats().refused, 1);
        r.set_epoch(5);
        assert_eq!(r.on_datagram(0, &d[0]), Ok(Outcome::Stored));
        assert!(r.poll_frame(0).is_some());
        // a duplicate of a packet in reassembly
        let d2 = p.packetise(&frame(3000, true, 2), 0).unwrap();
        assert_eq!(r.on_datagram(0, &d2[0]), Ok(Outcome::Stored));
        assert_eq!(r.on_datagram(0, &d2[0]), Ok(Outcome::Duplicate));
    }

    #[test]
    fn payload_of_another_length_is_refused() {
        let mut p = Packetiser::new(1000, 0);
        let mut r = Reassembler::new(0);
        let d = p.packetise(&frame(3000, true, 1), 0).unwrap();
        r.on_datagram(0, &d[0]).unwrap();
        let mut short = d[1].clone();
        short.truncate(short.len() - 10);
        assert_eq!(r.on_datagram(0, &short), Err(Refusal::PayloadLength));
    }

    #[test]
    fn keeps_at_most_four_frames() {
        let mut p = Packetiser::new(1000, 0);
        let mut r = Reassembler::new(0);
        for i in 0..6 {
            let d = p.packetise(&frame(3000, i == 0, i as u8), 0).unwrap();
            r.on_datagram(i as u64, &d[0]).unwrap(); // one packet each, none complete
        }
        assert_eq!(r.in_flight().0, 4);
        assert_eq!(r.stats().frames_dropped, 2);
        assert!(r.poll_key_request(10).is_some());
    }

    #[test]
    fn stays_inside_the_byte_limit() {
        let cfg = Config {
            max_bytes: 10_000,
            ..Config::default()
        };
        let mut p = Packetiser::new(1000, 0);
        let mut r = Reassembler::with_config(0, cfg);
        for i in 0..4 {
            let d = p.packetise(&frame(20_000, i == 0, i as u8), 0).unwrap();
            for x in &d[..6] {
                r.on_datagram(0, x).unwrap();
            }
            assert!(r.in_flight().1 <= 10_000);
        }
    }

    #[test]
    fn counts_received_lost_and_recovered() {
        let mut p = Packetiser::new(1000, 0);
        let mut r = Reassembler::new(0);
        let k = p.packetise(&frame(9000, true, 1), 20).unwrap(); // 10 data + 2 parity
        assert_eq!(k.len(), 12);
        let mut keep = k.clone();
        keep.remove(3);
        keep.remove(3); // two data packets gone
        feed(&mut r, 0, &keep);
        assert!(r.poll_frame(0).is_some());
        // settle the books
        r.poll_frame(200_000);
        let i = r.take_interval();
        assert_eq!(
            i,
            Interval {
                received: 10,
                lost: 2,
                recovered: 2
            }
        );
        assert_eq!(r.take_interval(), Interval::default());
    }

    #[test]
    fn frame_numbers_wrap() {
        let mut r = Reassembler::new(0);
        let mut q = Packetiser::new(1000, 0);
        q.set_next_frame(u32::MAX - 1);
        feed(&mut r, 0, &q.packetise(&frame(100, true, 1), 0).unwrap());
        assert_eq!(r.poll_frame(0).unwrap().frame, u32::MAX - 1);
        feed(&mut r, 0, &q.packetise(&frame(100, false, 2), 0).unwrap());
        assert_eq!(r.poll_frame(0).unwrap().frame, u32::MAX);
        feed(&mut r, 0, &q.packetise(&frame(100, false, 3), 0).unwrap());
        assert_eq!(r.poll_frame(0).unwrap().frame, 0);
        feed(&mut r, 0, &q.packetise(&frame(100, false, 4), 0).unwrap());
        assert_eq!(r.poll_frame(0).unwrap().frame, 1);
    }
}
