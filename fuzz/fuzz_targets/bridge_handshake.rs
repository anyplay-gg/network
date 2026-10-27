//! The start of a bridge connection, as a game process may send it: the bytes before the
//! handshake is done are read with the small limit, and a proof is accepted only if it is the
//! proof. The first two bytes pick the size of the pieces and the length of the token.

#![no_main]

use anyplay_bridge_protocol::{Decoder, MAX_HELLO_PAYLOAD, NONCE_LEN, Role, proof, same, verify};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&piece, rest)) = data.split_first() else {
        return;
    };
    let Some((&token_len, rest)) = rest.split_first() else {
        return;
    };
    let (token, wire) = rest.split_at(usize::from(token_len).min(rest.len()));

    // A proof verifies for its own role and nothing else, and the comparison agrees with ==.
    let bridge_nonce = [piece; NONCE_LEN];
    let service_nonce = [token_len; NONCE_LEN];
    let given = proof(token, Role::Bridge, &bridge_nonce, &service_nonce);
    assert!(verify(
        token,
        Role::Bridge,
        &bridge_nonce,
        &service_nonce,
        &given
    ));
    assert!(!verify(
        token,
        Role::Service,
        &bridge_nonce,
        &service_nonce,
        &given
    ));
    assert_eq!(
        verify(token, Role::Bridge, &bridge_nonce, &service_nonce, wire),
        wire == given.as_slice()
    );
    assert_eq!(same(wire, &given), wire == given.as_slice());

    // Until the handshake is done no frame is bigger than the small limit, and an error ends it.
    let mut decoder = Decoder::with_max_payload(MAX_HELLO_PAYLOAD);
    for chunk in wire.chunks(usize::from(piece % 64) + 1) {
        decoder.push(chunk);
        loop {
            match decoder.next_frame() {
                Ok(Some(frame)) => {
                    assert!(frame.payload.len() <= MAX_HELLO_PAYLOAD);
                    let _ = frame.message();
                }
                Ok(None) => break,
                Err(_) => return,
            }
        }
    }
});
