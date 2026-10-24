//! A voice datagram from another member of a party: read and written back to the same bytes, or
//! refused for exactly the reason its length, version, flags or payload give.

#![no_main]

use anyplay_voice::wire::{Frame, HEADER_LEN, MAX_PAYLOAD, VERSION, WireError};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    match Frame::parse(data) {
        Ok(frame) => {
            assert_eq!(frame.encode(), data);
            assert!(!frame.payload.is_empty() && frame.payload.len() <= MAX_PAYLOAD);
            assert_eq!(frame.payload, &data[HEADER_LEN..]);
        }
        Err(WireError::TooShort) => assert!(data.len() < HEADER_LEN),
        Err(WireError::Version(v)) => assert!(data.len() >= HEADER_LEN && v != VERSION),
        Err(WireError::Flags(f)) => assert!(data[0] == VERSION && f & !1 != 0),
        Err(WireError::EmptyPayload) => assert_eq!(data.len(), HEADER_LEN),
        Err(WireError::PayloadTooLong) => assert!(data.len() > HEADER_LEN + MAX_PAYLOAD),
    }
});
