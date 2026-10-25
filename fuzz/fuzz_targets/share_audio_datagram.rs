//! An audio datagram of share play from another member: read and written back to the same
//! bytes, or refused for exactly the reason its length, version, type, flags or payload give.

#![no_main]

use anyplay_share::wire::{
    AUDIO_HEADER_LEN, AudioDatagram, MAX_AUDIO_PAYLOAD, Refusal, TYPE_AUDIO, VERSION,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    match AudioDatagram::parse(data) {
        Ok(d) => {
            assert_eq!(d.encode().unwrap(), data);
            assert_eq!(d.payload, &data[AUDIO_HEADER_LEN..]);
            assert!(!d.payload.is_empty() && d.payload.len() <= MAX_AUDIO_PAYLOAD);
        }
        Err(Refusal::TooShort) => assert!(data.len() < AUDIO_HEADER_LEN + 1),
        Err(Refusal::Version(v)) => assert!(data[0] == v && v != VERSION),
        Err(Refusal::Type(t)) => assert!(data[1] == t && t != TYPE_AUDIO),
        Err(Refusal::Flags(f)) => assert!(data[2] == f && f & !1 != 0),
        Err(Refusal::PayloadTooLong) => assert!(data.len() > AUDIO_HEADER_LEN + MAX_AUDIO_PAYLOAD),
        Err(_) => unreachable!(),
    }
});
