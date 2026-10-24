//! One payload of a bridge frame, as a game process may send it: the first two bytes pick the
//! kind. A payload is read or refused, never a crash, and a message read from it is written back
//! so that it reads the same.

#![no_main]

use anyplay_bridge_protocol::{Kind, Message};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((kind, payload)) = data.split_first_chunk::<2>() else {
        return;
    };
    let kind = Kind(u16::from_le_bytes(*kind));
    let Ok(Some(message)) = Message::parse(kind, payload) else {
        return;
    };
    assert_eq!(message.kind(), kind);
    let again = message
        .payload()
        .expect("a message read from a payload can be written");
    assert_eq!(Message::parse(kind, &again), Ok(Some(message)));
});
