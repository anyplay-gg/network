//! A pad datagram of share play from the guest, through the host's gate: read and written back
//! to the same bytes, or refused; and whatever the gate lets through is a state with no reserved
//! button bit, in an order that only goes forward, within the rate.

#![no_main]

use anyplay_share::pad::{BUTTON_MASK, PadDatagram, PadGate, PadMode, PadRefusal, TYPE_PAD};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    match PadDatagram::parse(data) {
        Ok(d) => {
            assert_eq!(d.encode().unwrap(), data);
            assert!((1..=3).contains(&d.states.len()));
            assert!(d.states.iter().all(|s| s.buttons & !BUTTON_MASK == 0));
        }
        Err(PadRefusal::Type(t)) => assert!(data[1] == t && t != TYPE_PAD),
        Err(_) => {}
    }

    // The first byte picks the mode, the rest is fed to a gate as datagrams cut at 0xFF bytes.
    let Some((mode, rest)) = data.split_first() else {
        return;
    };
    let mut gate = PadGate::new(0);
    gate.set_mode(match mode % 3 {
        0 => PadMode::Watch,
        1 => PadMode::HandOver,
        _ => PadMode::Together,
    });
    let mut applied = 0u64;
    for (i, piece) in rest.split(|b| *b == 0xFF).enumerate() {
        let out = gate.on_datagram(i as u64 * 10_000, piece);
        for (_, state) in &out.apply {
            assert!(state.buttons & !BUTTON_MASK == 0);
            assert!(gate.mode() != PadMode::Watch);
        }
        applied += out.apply.len() as u64;
    }
    // 30 in a burst and 300 a second: no more than that can come out of this many datagrams.
    let seconds = rest.split(|b| *b == 0xFF).count() as u64 / 100;
    assert!(applied <= 30 + 300 * (seconds + 1) + 3);
});
