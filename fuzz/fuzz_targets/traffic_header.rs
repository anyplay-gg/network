//! A game datagram, or the start of a game stream: the header in front is read from exactly its
//! eight bytes, and the game's bytes after it are left as they came.

#![no_main]

use anyplay_bridge_protocol::TrafficHeader;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    match TrafficHeader::split(data) {
        Some((header, game)) => {
            assert_eq!(header.encode()[..], data[..TrafficHeader::LEN]);
            assert_eq!(game, &data[TrafficHeader::LEN..]);
        }
        None => assert!(data.len() < TrafficHeader::LEN),
    }
});
