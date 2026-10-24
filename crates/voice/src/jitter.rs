//! The jitter buffer of one peer: puts frames back in order, decides how long to wait for them,
//! and says for every 20 ms tick what the decoder should do. It holds Opus packets, not audio,
//! so it can be tested without a codec.

use std::collections::BTreeMap;
use std::time::Instant;

/// Samples per frame at 48 kHz.
pub(crate) const FRAME_SAMPLES: usize = 960;
const FRAME_MS: f64 = 20.0;

/// The shortest and longest wait before a talk starts playing, in frames.
const MIN_DEPTH: usize = 2;
const MAX_DEPTH: usize = 10;
/// Extra frames the buffer may hold above the depth before it skips ahead.
const SLACK: usize = 6;
/// A frame that is missing while later ones are here is waited for this many ticks if the buffer
/// is thinner than its depth, which is how the playout delay grows when the network gets rougher.
const MAX_HOLD: u32 = 4;
/// Ticks between two frames dropped to shrink a buffer that holds more than it needs.
const ACCELERATE_EVERY: u32 = 25;
/// Ticks of concealment after a lost or late frame before the stream is given up as ended.
const MAX_CONCEAL: u32 = 8;
/// The same after a packet that opened a stretch of comfort noise (DTX), which the sender then
/// repeats every 400 ms.
const MAX_CONCEAL_AFTER_DTX: u32 = 26;
/// Extra depth that late frames can add.
const MAX_BUMP: u32 = 4;
/// Ticks without trouble after which the extra depth shrinks by one frame.
const CALM_TICKS: u32 = 150;

/// What the decoder does for the next 20 ms.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Playout {
    /// Nothing is being said.
    Silence,
    /// Decode this packet.
    Frame { payload: Vec<u8>, end_of_talk: bool },
    /// The frame before this packet was lost; rebuild it from the redundancy in this packet. The
    /// packet itself is played on the next tick.
    Recover { next_payload: Vec<u8> },
    /// A frame is missing and nothing can rebuild it; let the codec fill in.
    Conceal,
}

struct Entry {
    timestamp: u32,
    end_of_talk: bool,
    payload: Vec<u8>,
}

enum State {
    /// Waiting for enough frames to start. `waited` counts the ticks since the first arrived.
    Idle {
        waited: usize,
    },
    Playing(Playing),
}

struct Playing {
    /// The sequence number due next.
    next: i64,
    /// The timestamp of the slot played last, if a frame of this stretch has played.
    last_ts: Option<u32>,
    concealed: u32,
    after_dtx: bool,
}

/// What the buffer saw, for a UI or a log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PeerStats {
    /// Frames accepted into the buffer.
    pub received: u64,
    /// Frames that arrived twice.
    pub duplicates: u64,
    /// Frames that arrived after their time had passed.
    pub late: u64,
    /// Ticks rebuilt from the redundancy in the next frame.
    pub recovered: u64,
    /// Ticks filled in by the codec because nothing was there.
    pub concealed: u64,
    /// Frames dropped to catch up after a burst.
    pub skipped: u64,
    /// The depth, in frames, the buffer waits before it starts a talk.
    pub depth: u32,
}

pub(crate) struct JitterBuffer {
    frames: BTreeMap<i64, Entry>,
    highest: Option<i64>,
    state: State,
    /// Smoothed interarrival jitter in milliseconds (RFC 3550).
    jitter_ms: f64,
    previous: Option<(Instant, u32)>,
    bump: u32,
    calm: u32,
    accelerate_wait: u32,
    stats: PeerStats,
}

impl JitterBuffer {
    pub(crate) fn new() -> JitterBuffer {
        let mut b = JitterBuffer {
            frames: BTreeMap::new(),
            highest: None,
            state: State::Idle { waited: 0 },
            jitter_ms: 0.0,
            previous: None,
            bump: 0,
            calm: 0,
            accelerate_wait: 0,
            stats: PeerStats::default(),
        };
        b.stats.depth = b.depth() as u32;
        b
    }

    pub(crate) fn stats(&self) -> PeerStats {
        PeerStats {
            depth: self.depth() as u32,
            ..self.stats
        }
    }

    fn depth(&self) -> usize {
        let estimate = 1 + (3.0 * self.jitter_ms / FRAME_MS).ceil() as usize;
        (estimate + self.bump as usize).clamp(MIN_DEPTH, MAX_DEPTH)
    }

    /// Take in a frame that arrived at `now`.
    pub(crate) fn insert(
        &mut self,
        sequence: u16,
        timestamp: u32,
        end_of_talk: bool,
        payload: &[u8],
        now: Instant,
    ) {
        self.measure(timestamp, now);

        // While nothing waits and nothing plays, the sender may have restarted at any number.
        let anchored = matches!(self.state, State::Playing(_)) || !self.frames.is_empty();
        let extended = match self.highest {
            Some(h) if anchored => h + i64::from(sequence.wrapping_sub(h as u16) as i16),
            _ => i64::from(sequence),
        };

        if let State::Playing(p) = &self.state
            && extended < p.next
        {
            self.stats.late += 1;
            self.bump = (self.bump + 1).min(MAX_BUMP);
            self.calm = 0;
            return;
        }
        if self.frames.contains_key(&extended) {
            self.stats.duplicates += 1;
            return;
        }
        self.frames.insert(
            extended,
            Entry {
                timestamp,
                end_of_talk,
                payload: payload.to_vec(),
            },
        );
        self.highest = Some(
            self.highest
                .filter(|_| anchored)
                .map_or(extended, |h| h.max(extended)),
        );
        self.stats.received += 1;
    }

    /// RFC 3550 interarrival jitter, on the sender's sample clock.
    fn measure(&mut self, timestamp: u32, now: Instant) {
        if let Some((at, ts)) = self.previous {
            let arrived = now.saturating_duration_since(at).as_secs_f64() * 1000.0;
            let sent = f64::from(timestamp.wrapping_sub(ts) as i32) / 48.0;
            let d = (arrived - sent).abs().min(500.0);
            self.jitter_ms += (d - self.jitter_ms) / 16.0;
        }
        self.previous = Some((now, timestamp));
    }

    /// What to play for the next 20 ms.
    pub(crate) fn pop(&mut self) -> Playout {
        let depth = self.depth();
        if let State::Idle { waited } = &mut self.state {
            if self.frames.is_empty() {
                return Playout::Silence;
            }
            *waited += 1;
            let ready = self.frames.len() >= depth
                || *waited >= depth
                || self.frames.values().any(|e| e.end_of_talk);
            if !ready {
                return Playout::Silence;
            }
            let first = *self.frames.keys().next().expect("not empty");
            self.state = State::Playing(Playing {
                next: first,
                last_ts: None,
                concealed: 0,
                after_dtx: false,
            });
        }
        self.pop_playing()
    }

    fn pop_playing(&mut self) -> Playout {
        let depth = self.depth();

        // After a burst, drop the oldest frames rather than carry the delay for good.
        let mut trimmed = false;
        while self.frames.len() > depth + SLACK {
            self.frames.pop_first();
            self.stats.skipped += 1;
            trimmed = true;
        }
        let State::Playing(p) = &mut self.state else {
            unreachable!("pop_playing only runs while playing");
        };
        if trimmed && let Some((&first, _)) = self.frames.first_key_value() {
            // Continue from what is left.
            p.next = first;
            p.last_ts = None;
        }

        if let Some(entry) = self.frames.remove(&p.next) {
            if let Some(last) = p.last_ts {
                let expected = last.wrapping_add(FRAME_SAMPLES as u32);
                if (entry.timestamp.wrapping_sub(expected) as i32) > 0 {
                    // Silence came in between (the sender sent nothing): this frame opens a new
                    // talk, which waits for its own depth.
                    self.frames.insert(p.next, entry);
                    self.state = State::Idle { waited: 0 };
                    return Playout::Silence;
                }
            }
            p.last_ts = Some(entry.timestamp);
            p.next += 1;
            p.concealed = 0;
            p.after_dtx = entry.payload.len() <= 2;
            self.calm += 1;
            if self.calm >= CALM_TICKS {
                self.calm = 0;
                self.bump = self.bump.saturating_sub(1);
            }
            self.accelerate_wait = self.accelerate_wait.saturating_sub(1);
            if self.accelerate_wait == 0 && self.frames.len() > depth + 2 {
                // More waiting than the depth calls for: drop the next frame to catch up.
                if let Some(extra) = self.frames.remove(&p.next) {
                    p.last_ts = Some(extra.timestamp);
                    p.next += 1;
                    self.stats.skipped += 1;
                    self.accelerate_wait = ACCELERATE_EVERY;
                }
            }
            if entry.end_of_talk {
                self.state = State::Idle { waited: 0 };
            }
            return Playout::Frame {
                payload: entry.payload,
                end_of_talk: entry.end_of_talk,
            };
        }

        // The frame due is not here.
        p.concealed += 1;
        let limit = if p.after_dtx {
            MAX_CONCEAL_AFTER_DTX
        } else {
            MAX_CONCEAL
        };
        if p.concealed > limit {
            self.state = State::Idle { waited: 0 };
            return Playout::Silence;
        }
        let due = p.next;
        if self.frames.range(due + 1..).next().is_none() {
            // Nothing behind it either. It may be late, or the sender may have gone quiet
            // without a word (DTX), so the slot stays open and the network gets more time.
            if p.concealed == 1 {
                self.bump = (self.bump + 1).min(MAX_BUMP);
                self.calm = 0;
            }
            self.stats.concealed += 1;
            return Playout::Conceal;
        }
        let later = self.frames.range(due + 1..).count();
        if later + 1 < depth && p.concealed <= MAX_HOLD {
            // The buffer is thinner than it should be: give the frame a little longer.
            self.stats.concealed += 1;
            return Playout::Conceal;
        }
        // Later frames are here, so this one is lost: close its slot.
        let slot = p.last_ts.map(|t| t.wrapping_add(FRAME_SAMPLES as u32));
        p.next += 1;
        p.last_ts = slot;
        if let (Some(entry), Some(slot)) = (self.frames.get(&(due + 1)), slot)
            && entry.timestamp == slot.wrapping_add(FRAME_SAMPLES as u32)
        {
            self.stats.recovered += 1;
            return Playout::Recover {
                next_payload: entry.payload.clone(),
            };
        }
        self.stats.concealed += 1;
        Playout::Conceal
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A fake sender and network: frames are sent every 20 ms and arrive when the test says.
    struct Rig {
        jb: JitterBuffer,
        t0: Instant,
        played: Vec<Playout>,
        tick: u32,
    }

    fn payload(seq: u16) -> Vec<u8> {
        vec![(seq & 0xff) as u8, 9, 9, 9]
    }

    impl Rig {
        fn new() -> Rig {
            Rig {
                jb: JitterBuffer::new(),
                t0: Instant::now(),
                played: Vec::new(),
                tick: 0,
            }
        }

        fn arrive(&mut self, seq: u16, at_ms: u64, eot: bool) {
            self.jb.insert(
                seq,
                u32::from(seq) * 960,
                eot,
                &payload(seq),
                self.t0 + Duration::from_millis(at_ms),
            );
        }

        fn tick(&mut self) {
            self.tick += 1;
            let p = self.jb.pop();
            self.played.push(p);
        }

        fn frames_played(&self) -> usize {
            self.played
                .iter()
                .filter(|p| matches!(p, Playout::Frame { .. }))
                .count()
        }

        fn payloads(&self) -> Vec<u8> {
            self.played
                .iter()
                .filter_map(|p| match p {
                    Playout::Frame { payload, .. } => Some(payload[0]),
                    _ => None,
                })
                .collect()
        }
    }

    /// Send `n` frames 20 ms apart, deliver them through `schedule` (sequence number to arrival
    /// times in ms, empty meaning lost), and tick every 20 ms.
    fn run(n: u16, schedule: impl Fn(u16) -> Vec<u64>) -> Rig {
        let mut rig = Rig::new();
        let mut arrivals: Vec<(u64, u16)> = (0..n)
            .flat_map(|s| schedule(s).into_iter().map(move |t| (t, s)))
            .collect();
        arrivals.sort();
        let mut i = 0;
        for tick in 0..(u64::from(n) + 40) {
            let now = tick * 20;
            while i < arrivals.len() && arrivals[i].0 <= now {
                rig.arrive(arrivals[i].1, arrivals[i].0, arrivals[i].1 == n - 1);
                i += 1;
            }
            rig.tick();
        }
        rig
    }

    #[test]
    fn plays_a_clean_stream_in_order_and_complete() {
        let rig = run(100, |s| vec![u64::from(s) * 20]);
        assert_eq!(rig.frames_played(), 100);
        assert_eq!(
            rig.payloads(),
            (0..100u16).map(|s| s as u8).collect::<Vec<_>>()
        );
        assert_eq!(rig.jb.stats().concealed + rig.jb.stats().recovered, 0);
    }

    #[test]
    fn puts_reordered_frames_back() {
        // Every fifth frame is delayed by 30 ms and arrives behind its successor.
        let rig = run(200, |s| {
            let base = u64::from(s) * 20;
            vec![if s % 5 == 2 { base + 30 } else { base }]
        });
        // It learns: the first delayed frames may miss their time, the later ones do not.
        let stats = rig.jb.stats();
        assert!(stats.late < 15, "{stats:?}");
        assert!(stats.depth >= 3, "{stats:?}");
        let payloads = rig.payloads();
        assert!(payloads.len() >= 185, "{} {stats:?}", payloads.len());
        assert!(
            payloads.windows(2).all(|w| w[0] < w[1] || w[0] > 240),
            "out of order"
        );
    }

    #[test]
    fn rebuilds_or_conceals_every_lost_frame() {
        for (loss_percent, seed) in [(10u32, 7u32), (30, 11)] {
            let mut x = seed;
            let lost: Vec<bool> = (0..400)
                .map(|_| {
                    x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (x >> 8) % 100 < loss_percent
                })
                .collect();
            let rig = run(400, |s| {
                if lost[s as usize] {
                    vec![]
                } else {
                    vec![u64::from(s) * 20]
                }
            });
            let stats = rig.jb.stats();
            let lost_count = lost.iter().filter(|l| **l).count() as u64;
            // The stream is cut at its last frame, and a lost tail is just an end.
            let tail = lost.iter().rev().take_while(|l| **l).count() as u64;
            let accounted = rig.frames_played() as u64 + stats.recovered + stats.concealed;
            assert!(
                accounted + tail + 1 >= 400,
                "{loss_percent}% loss: {accounted} of 400 slots accounted for, {stats:?}"
            );
            assert!(stats.recovered + stats.concealed + tail + 1 >= lost_count);
            // Isolated losses are rebuilt from the next frame, so at 10 % most are.
            if loss_percent == 10 {
                assert!(stats.recovered > stats.concealed, "{stats:?}");
            }
        }
    }

    #[test]
    fn ignores_duplicates() {
        let rig = run(50, |s| vec![u64::from(s) * 20, u64::from(s) * 20 + 5]);
        assert_eq!(rig.frames_played(), 50);
        assert_eq!(rig.jb.stats().duplicates, 50);
    }

    #[test]
    fn drops_late_frames_and_waits_longer_afterwards() {
        let mut rig = Rig::new();
        for s in 0..10u16 {
            rig.arrive(s, u64::from(s) * 20, false);
            rig.tick();
        }
        let before = rig.jb.stats().depth;
        // Frame 10 never comes on time; 11 and 12 do. Frame 10 then shows up long after.
        rig.arrive(11, 220, false);
        rig.arrive(12, 240, false);
        for _ in 0..4 {
            rig.tick();
        }
        rig.arrive(10, 300, false);
        let stats = rig.jb.stats();
        assert_eq!(stats.late, 1);
        assert!(stats.depth > before, "{before} -> {}", stats.depth);
        assert!(!rig.payloads().contains(&10));
    }

    #[test]
    fn adapts_its_depth_to_the_jitter() {
        let calm = run(150, |s| vec![u64::from(s) * 20]);
        // Frames wander by up to 60 ms.
        let jittery = run(150, |s| {
            let wobble = [0u64, 55, 10, 40, 5, 60][s as usize % 6];
            vec![u64::from(s) * 20 + wobble]
        });
        assert!(
            jittery.jb.stats().depth > calm.jb.stats().depth,
            "{} vs {}",
            jittery.jb.stats().depth,
            calm.jb.stats().depth
        );
        assert!(jittery.jb.stats().depth <= MAX_DEPTH as u32);
    }

    #[test]
    fn end_of_talk_ends_the_stream_at_once() {
        let mut rig = Rig::new();
        for s in 0..6u16 {
            rig.arrive(s, u64::from(s) * 20, s == 5);
            rig.tick();
        }
        for _ in 0..8 {
            rig.tick();
        }
        assert_eq!(rig.frames_played(), 6);
        // After the last frame it is silence, not concealment.
        assert_eq!(rig.jb.stats().concealed, 0);
        assert!(matches!(rig.played.last(), Some(Playout::Silence)));
    }

    #[test]
    fn a_stream_that_just_stops_is_given_up_after_a_while() {
        let mut rig = Rig::new();
        for s in 0..6u16 {
            rig.arrive(s, u64::from(s) * 20, false);
        }
        for _ in 0..30 {
            rig.tick();
        }
        assert_eq!(rig.jb.stats().concealed, u64::from(MAX_CONCEAL));
        assert!(matches!(rig.played.last(), Some(Playout::Silence)));
    }

    #[test]
    fn a_gap_in_the_timestamps_starts_a_new_talk() {
        let mut rig = Rig::new();
        for s in 0..6u16 {
            rig.arrive(s, u64::from(s) * 20, false);
        }
        // The sender went quiet for a second (DTX), then sent sequence 6 with a later timestamp.
        rig.jb
            .insert(6, 5 * 960 + 960 + 48_000, false, &payload(6), rig.t0);
        rig.jb
            .insert(7, 5 * 960 + 2 * 960 + 48_000, true, &payload(7), rig.t0);
        for _ in 0..20 {
            rig.tick();
        }
        assert_eq!(rig.frames_played(), 8);
        assert_eq!(rig.jb.stats().concealed, 0);
    }

    #[test]
    fn follows_the_sequence_number_across_the_wrap() {
        let mut rig = Rig::new();
        for (i, s) in [65_533u16, 65_534, 65_535, 0, 1, 2].into_iter().enumerate() {
            rig.jb.insert(
                s,
                (i as u32) * 960,
                s == 2,
                &payload(s),
                rig.t0 + Duration::from_millis(i as u64 * 20),
            );
            rig.tick();
        }
        for _ in 0..8 {
            rig.tick();
        }
        assert_eq!(rig.frames_played(), 6);
        assert_eq!(rig.jb.stats().late, 0);
    }

    #[test]
    fn a_burst_after_a_stall_does_not_keep_the_delay() {
        let mut rig = Rig::new();
        rig.arrive(0, 0, false);
        rig.arrive(1, 20, false);
        for _ in 0..3 {
            rig.tick();
        }
        for s in 2..60u16 {
            rig.arrive(s, 2_000 + u64::from(s), false);
        }
        for _ in 0..80 {
            rig.tick();
        }
        assert!(rig.jb.stats().skipped > 0);
    }
}
