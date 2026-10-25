//! The share engine end to end without a network: a test picture goes through the test
//! encoder, the packetiser, a shaped in-memory link (loss, duplicates, reordering, delay), the
//! reassembler and the test decoder. The frame number is read back out of the pixels, so a
//! damaged or misplaced picture cannot pass. Time is simulated.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use anyplay_share::codec::VideoEncoder;
use anyplay_share::packetiser::Packetiser;
use anyplay_share::reassembler::{Config, Reassembler, Stats};
use anyplay_share::testcodec::{FRAME_INTERVAL_US, TestDecoder, TestEncoder, TestPattern};
use anyplay_share::wire::{Datagram, VideoDatagram};

const W: u32 = 160;
const H: u32 = 90;
/// A datagram of 1200 bytes, 20 of them header.
const PAYLOAD: usize = 1180;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn chance(&mut self, p: f64) -> bool {
        ((self.next() >> 11) as f64 / (1u64 << 53) as f64) < p
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
}

#[derive(Clone, Copy)]
struct Shape {
    loss: f64,
    dup: f64,
    /// Chance that a packet is held back by up to `reorder_us` more.
    reorder: f64,
    reorder_us: u64,
    delay_us: u64,
    jitter_us: u64,
}

impl Shape {
    const CLEAN: Shape = Shape {
        loss: 0.0,
        dup: 0.0,
        reorder: 0.0,
        reorder_us: 0,
        delay_us: 30_000,
        jitter_us: 0,
    };
}

struct Link {
    shape: Shape,
    rng: Rng,
    heap: BinaryHeap<(Reverse<u64>, u64, Vec<u8>)>,
    seq: u64,
}

impl Link {
    fn new(shape: Shape, seed: u64) -> Link {
        Link {
            shape,
            rng: Rng(seed),
            heap: BinaryHeap::new(),
            seq: 0,
        }
    }

    fn send(&mut self, now: u64, datagram: Vec<u8>) {
        if self.rng.chance(self.shape.loss) {
            return;
        }
        let copies = 1 + usize::from(self.rng.chance(self.shape.dup));
        for _ in 0..copies {
            let mut at = now + self.shape.delay_us + self.rng.below(self.shape.jitter_us + 1);
            if self.rng.chance(self.shape.reorder) {
                at += self.rng.below(self.shape.reorder_us + 1);
            }
            self.seq += 1;
            self.heap.push((Reverse(at), self.seq, datagram.clone()));
        }
    }

    fn deliver(&mut self, now: u64) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while self
            .heap
            .peek()
            .is_some_and(|(Reverse(at), _, _)| *at <= now)
        {
            out.push(self.heap.pop().unwrap().2);
        }
        out
    }
}

struct Run {
    /// (frame number read from the pixels, key frame)
    delivered: Vec<(u32, bool)>,
    key_requests: u32,
    stats: Stats,
    sent: u32,
}

impl Run {
    fn ratio(&self) -> f64 {
        self.delivered.len() as f64 / f64::from(self.sent)
    }
}

struct Setup {
    frames: u32,
    shape: Shape,
    parity: u8,
    key_interval: u32,
    seed: u64,
}

/// Run a stream. `sabotage(frame, packet index, packets in frame)` drops a datagram before the
/// link sees it.
fn run(setup: Setup, sabotage: impl Fn(u32, usize, usize) -> bool) -> Run {
    let mut pattern = TestPattern::new(W, H);
    let mut encoder = TestEncoder::new(setup.key_interval);
    let mut packetiser = Packetiser::new(PAYLOAD, 0);
    let mut link = Link::new(setup.shape, setup.seed);
    let mut reassembler = Reassembler::new(0);
    let mut decoder = TestDecoder::new();

    let mut delivered: Vec<(u32, bool)> = Vec::new();
    let mut key_requests = 0;
    let mut key_arrives_at: Option<u64> = None;
    // The control stream takes a little while to carry the key request to the host.
    let control_us = 20_000;

    let tick = |t: u64,
                link: &mut Link,
                reassembler: &mut Reassembler,
                decoder: &mut TestDecoder,
                delivered: &mut Vec<(u32, bool)>,
                key_requests: &mut u32,
                key_arrives_at: &mut Option<u64>| {
        for d in link.deliver(t) {
            let _ = reassembler.on_datagram(t, &d);
        }
        while let Some(f) = reassembler.poll_frame(t) {
            let (picture, number) = decoder
                .decode_numbered(&f.encoded)
                .expect("the reassembler handed the decoder a frame it cannot decode");
            let number = number.expect("frame number in the pixels");
            assert_eq!(number, f.frame, "picture and frame number agree");
            assert_eq!(
                picture,
                TestPattern::render(W, H, number),
                "damaged picture {number}"
            );
            if let Some(&(last, _)) = delivered.last() {
                assert!(number > last, "frame {number} after {last}");
            }
            delivered.push((number, f.encoded.key));
        }
        if reassembler.poll_key_request(t).is_some() {
            *key_requests += 1;
            key_arrives_at.get_or_insert(t + control_us);
        }
    };

    for i in 0..setup.frames {
        let now = u64::from(i) * u64::from(FRAME_INTERVAL_US);
        if key_arrives_at.is_some_and(|at| at <= now) {
            encoder.request_key();
            key_arrives_at = None;
        }
        let picture = pattern.next_frame();
        let encoded = encoder.encode(&picture, false).unwrap();
        let datagrams = packetiser.packetise(&encoded, setup.parity).unwrap();
        let n = datagrams.len();
        for (idx, d) in datagrams.into_iter().enumerate() {
            if !sabotage(i, idx, n) {
                link.send(now, d);
            }
        }
        for step in 0..4 {
            tick(
                now + step * 4_000,
                &mut link,
                &mut reassembler,
                &mut decoder,
                &mut delivered,
                &mut key_requests,
                &mut key_arrives_at,
            );
        }
    }
    // let the last packets arrive and the timeouts run
    let end = u64::from(setup.frames) * u64::from(FRAME_INTERVAL_US);
    for step in 0..100 {
        tick(
            end + step * 4_000,
            &mut link,
            &mut reassembler,
            &mut decoder,
            &mut delivered,
            &mut key_requests,
            &mut key_arrives_at,
        );
    }
    Run {
        delivered,
        key_requests,
        stats: reassembler.stats(),
        sent: setup.frames,
    }
}

fn setup(frames: u32, shape: Shape, parity: u8, seed: u64) -> Setup {
    Setup {
        frames,
        shape,
        parity,
        key_interval: 30,
        seed,
    }
}

#[test]
fn without_loss_every_frame_arrives_identically() {
    let r = run(setup(300, Shape::CLEAN, 0, 1), |_, _, _| false);
    assert_eq!(r.delivered.len(), 300);
    assert!(r.delivered.iter().enumerate().all(|(i, d)| d.0 == i as u32));
    assert_eq!(r.key_requests, 0);
    assert_eq!(r.stats.recovered, 0);
    assert_eq!(r.stats.lost, 0);
    // key frames every 30
    assert_eq!(r.delivered.iter().filter(|d| d.1).count(), 10);
}

#[test]
fn duplicates_and_reordering_within_a_frame_cost_nothing() {
    let shape = Shape {
        dup: 0.15,
        reorder: 0.3,
        reorder_us: 8_000,
        jitter_us: 3_000,
        ..Shape::CLEAN
    };
    let r = run(setup(300, shape, 10, 2), |_, _, _| false);
    assert_eq!(r.delivered.len(), 300);
    assert_eq!(r.key_requests, 0);
    assert!(r.stats.duplicates + r.stats.late > 0);
}

#[test]
fn reordering_across_frames_never_shows_a_wrong_picture() {
    let shape = Shape {
        reorder: 0.05,
        reorder_us: 25_000,
        jitter_us: 5_000,
        ..Shape::CLEAN
    };
    let r = run(setup(400, shape, 10, 3), |_, _, _| false);
    // `run` checks every picture; some frames may be given up, most arrive
    assert!(r.ratio() > 0.5, "{}", r.ratio());
}

#[test]
fn two_percent_loss_with_parity_delivers_nearly_everything_and_nothing_damaged() {
    let shape = Shape {
        loss: 0.02,
        jitter_us: 2_000,
        ..Shape::CLEAN
    };
    let with = run(setup(500, shape, 10, 4), |_, _, _| false);
    let without = run(setup(500, shape, 0, 4), |_, _, _| false);
    // every delivered picture was checked pixel by pixel inside `run`
    assert!(with.ratio() > 0.9, "with parity: {}", with.ratio());
    assert!(with.stats.recovered > 0);
    assert!(
        with.ratio() > without.ratio() + 0.1,
        "parity {} vs none {}",
        with.ratio(),
        without.ratio()
    );
    assert!(with.stats.lost > 0, "loss is counted");
}

#[test]
fn a_frame_that_cannot_be_completed_asks_for_a_key_frame_and_the_stream_recovers() {
    // frame 50 loses all but its first three packets, far more than its parity
    let r = run(setup(150, Shape::CLEAN, 10, 5), |frame, idx, _| {
        frame == 50 && idx >= 3
    });
    let numbers: Vec<u32> = r.delivered.iter().map(|d| d.0).collect();
    assert!(numbers.contains(&49));
    assert!(!numbers.contains(&50));
    assert!(r.key_requests >= 1);
    // the first picture after the loss is a key frame, and the stream goes on
    let after = r.delivered.iter().find(|d| d.0 > 50).expect("recovered");
    assert!(after.1, "first frame after the loss is a key frame");
    assert!(after.0 <= 58, "recovered at {}", after.0);
    assert_eq!(*numbers.last().unwrap(), 149);
    assert!(r.stats.frames_dropped >= 1);
}

#[test]
fn losing_the_key_frame_itself_is_survived() {
    // the very first key frame is mostly lost
    let r = run(setup(120, Shape::CLEAN, 10, 6), |frame, idx, _| {
        frame == 0 && idx % 2 == 0
    });
    assert!(r.key_requests >= 1);
    assert!(r.delivered.len() > 90, "{}", r.delivered.len());
    assert!(r.delivered[0].1);
}

#[test]
fn a_burst_of_loss_is_survived() {
    // three frames in a row lose half their packets
    let r = run(setup(200, Shape::CLEAN, 10, 7), |frame, idx, _| {
        (80..83).contains(&frame) && idx % 2 == 1
    });
    assert!(r.key_requests >= 1);
    assert!(r.delivered.iter().any(|d| d.0 > 100));
}

#[test]
fn garbage_datagrams_never_panic() {
    let mut rng = Rng(0xBAD5EED);
    let mut r = Reassembler::with_config(
        1,
        Config {
            max_bytes: 200_000,
            ..Config::default()
        },
    );
    let mut now = 0u64;
    let mut packetiser = Packetiser::new(1000, 1);
    let mut pattern = TestPattern::new(W, H);
    let mut encoder = TestEncoder::new(5);
    for round in 0..600u32 {
        now += 1_000 + rng.below(30_000);
        // pure noise
        let len = rng.below(1300) as usize;
        let noise: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        let _ = r.on_datagram(now, &noise);
        let _ = Datagram::parse(&noise);
        // noise with a plausible header
        let mut shaped = vec![0u8, 1, rng.below(4) as u8, 1];
        shaped.extend((0..16 + rng.below(1200)).map(|_| rng.next() as u8));
        shaped[13] = (shaped[13] % 9).max(1);
        shaped[12] %= shaped[13];
        let _ = r.on_datagram(now, &shaped);
        // real datagrams, damaged
        let encoded = encoder.encode(&pattern.next_frame(), false).unwrap();
        for mut d in packetiser.packetise(&encoded, 10).unwrap() {
            match rng.below(5) {
                0 => {
                    let at = rng.below(d.len() as u64) as usize;
                    d[at] ^= 1 << rng.below(8);
                }
                1 => d.truncate(rng.below(d.len() as u64) as usize),
                2 => d.extend((0..rng.below(50)).map(|_| rng.next() as u8)),
                3 => continue,
                _ => {}
            }
            if VideoDatagram::parse(&d).is_ok() || rng.chance(0.5) {
                let _ = r.on_datagram(now, &d);
            }
        }
        while r.poll_frame(now).is_some() {}
        let _ = r.poll_key_request(now);
        let (frames, bytes) = r.in_flight();
        assert!(frames <= 4, "round {round}: {frames} frames");
        assert!(bytes <= 200_000, "round {round}: {bytes} bytes");
    }
}
