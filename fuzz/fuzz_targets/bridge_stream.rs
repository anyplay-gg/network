//! The bytes of a bridge connection, arriving in pieces: the first byte sets how long each piece
//! is. Every frame the decoder hands out stays within the size limit and encodes back to itself;
//! an error ends the connection, as it does in the net service.

#![no_main]

use anyplay_bridge_protocol::{Decoder, MAX_PAYLOAD, Message};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&piece, wire)) = data.split_first() else {
        return;
    };
    let mut decoder = Decoder::new();
    for chunk in wire.chunks(usize::from(piece % 64) + 1) {
        decoder.push(chunk);
        loop {
            let frame = match decoder.next_frame() {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(_) => return,
            };
            assert!(frame.payload.len() <= MAX_PAYLOAD);

            let mut bytes = Vec::new();
            frame
                .encode(&mut bytes)
                .expect("a decoded frame can be encoded");
            let mut again = Decoder::new();
            again.push(&bytes);
            assert_eq!(again.next_frame(), Ok(Some(frame.clone())));
            assert_eq!(again.next_frame(), Ok(None));

            if let Ok(Some(message)) = frame.message() {
                let payload = message
                    .payload()
                    .expect("a message read from a frame can be written");
                assert_eq!(Message::parse(frame.kind, &payload), Ok(Some(message)));
            }
        }
    }
});
