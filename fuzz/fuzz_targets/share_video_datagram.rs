//! A video datagram of share play from another member: read and written back to the same bytes,
//! or refused for exactly the reason its length, version, type, flags or group layout give.

#![no_main]

use anyplay_share::wire::{
    MAX_DATA, MAX_GROUPS, MAX_PARITY, Refusal, TYPE_VIDEO, VERSION, VIDEO_HEADER_LEN, VideoDatagram,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    match VideoDatagram::parse(data) {
        Ok(d) => {
            assert_eq!(d.encode().unwrap(), data);
            assert_eq!(d.payload, &data[VIDEO_HEADER_LEN..]);
            assert!(!d.payload.is_empty());
            assert!((1..=MAX_GROUPS).contains(&d.groups) && d.group < d.groups);
            assert!((1..=MAX_DATA).contains(&d.data) && d.parity <= MAX_PARITY);
            assert!(d.data + d.parity <= 255 && d.index < d.data + d.parity);
            assert_eq!(d.is_parity, d.index >= d.data);
        }
        Err(Refusal::TooShort) => assert!(data.len() < VIDEO_HEADER_LEN + 1),
        Err(Refusal::Version(v)) => assert!(data[0] == v && v != VERSION),
        Err(Refusal::Type(t)) => assert!(data[1] == t && t != TYPE_VIDEO),
        Err(Refusal::Flags(f)) => assert!(data[2] == f && f & !3 != 0),
        Err(Refusal::Groups(g)) => assert!(data[13] == g && (g == 0 || g > MAX_GROUPS)),
        Err(Refusal::GroupIndex) => assert!(data[12] >= data[13]),
        Err(Refusal::DataCount(n)) => assert!(n == 0 || n > MAX_DATA),
        Err(Refusal::ParityCount(n)) => assert!(n > MAX_PARITY),
        Err(Refusal::GroupSize) => {}
        Err(Refusal::Index) => {}
        Err(Refusal::ParityFlag) => {}
        Err(Refusal::PayloadLength | Refusal::PayloadTooLong) => unreachable!(),
    }
});
