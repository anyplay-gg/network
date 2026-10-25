//! The share reassembler. The first byte picks a mode.
//!
//! * Even: the rest is a series of datagrams (a two-byte length each), fed with time passing.
//!   Nothing may panic and the reassembler stays inside four frames and 16 MiB.
//! * Odd: the rest is an encoded frame. It is packetised, up to its parity of packets per group
//!   is dropped, the rest shuffled in, and the reassembler must hand back exactly that frame.

#![no_main]

use anyplay_share::codec::EncodedFrame;
use anyplay_share::packetiser::Packetiser;
use anyplay_share::reassembler::{MAX_BYTES, MAX_FRAMES, Reassembler};
use anyplay_share::wire::VideoDatagram;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    if mode & 1 == 0 {
        garbage(rest);
    } else {
        round_trip(rest);
    }
});

fn garbage(mut rest: &[u8]) {
    let mut r = Reassembler::new(0);
    let mut now = 0u64;
    while rest.len() >= 3 {
        let len = ((usize::from(rest[0]) << 2) | usize::from(rest[1] & 3)).min(rest.len() - 2);
        now += u64::from(rest[1] >> 2) * 1_000;
        let (datagram, tail) = rest[2..].split_at(len);
        rest = tail;
        let _ = r.on_datagram(now, datagram);
        while r.poll_frame(now).is_some() {}
        let _ = r.poll_key_request(now);
        let (frames, bytes) = r.in_flight();
        assert!(frames <= MAX_FRAMES && bytes <= MAX_BYTES);
    }
}

fn round_trip(rest: &[u8]) {
    if rest.len() < 4 {
        return;
    }
    let (percent, payload, skip, body) = (
        rest[0] % 31,
        1000 + usize::from(rest[1]) * 4,
        rest[2],
        &rest[3..],
    );
    let frame = EncodedFrame {
        key: true,
        timestamp_us: 7,
        data: body.iter().cycle().take(body.len() * 40).copied().collect(),
    };
    let mut p = Packetiser::new(payload, 0);
    let Ok(mut datagrams) = p.packetise(&frame, percent) else {
        return;
    };
    // Drop up to each group's parity count of packets, starting at a place the input picks.
    let mut dropped = [0u16; 8];
    let n = datagrams.len();
    let mut keep = Vec::with_capacity(n);
    for (i, d) in datagrams.drain(..).enumerate() {
        let h = VideoDatagram::parse(&d).expect("the packetiser writes valid datagrams");
        let g = usize::from(h.group);
        if (i + usize::from(skip)) % 3 == 0 && dropped[g] < h.parity {
            dropped[g] += 1;
        } else {
            keep.push(d);
        }
    }
    // Reverse every other pair: the order of arrival does not matter.
    for pair in keep.chunks_mut(2) {
        if pair.len() == 2 && skip & 1 == 1 {
            pair.swap(0, 1);
        }
    }
    let mut r = Reassembler::new(0);
    for d in &keep {
        r.on_datagram(0, d).expect("valid datagram");
    }
    let got = r.poll_frame(0).expect("recoverable frame");
    assert_eq!(got.encoded, frame);
}
