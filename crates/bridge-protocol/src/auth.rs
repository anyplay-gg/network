//! The proofs of the bridge handshake.
//!
//! Each side proves that it knows the token of this game start without sending it: an
//! HMAC-SHA-256 keyed with the token over a context string, the proving side's role and both
//! sides' fresh nonces ([`proof`]). The token itself never crosses the connection. SHA-256 and
//! HMAC are written out here so that the crate keeps no dependencies; the tests check them against
//! the published vectors, and the specification's vectors are checked on both sides (the net
//! service and the bridge in the AnyPS5 fork).

/// The length of a nonce.
pub const NONCE_LEN: usize = 32;

/// The length of a proof.
pub const PROOF_LEN: usize = 32;

/// The context string at the start of every proof, so that a proof is useful for nothing else.
pub const CONTEXT: &[u8] = b"AnyPlay bridge v1";

/// A fresh random value one side contributes to the proofs.
pub type Nonce = [u8; NONCE_LEN];

/// An HMAC-SHA-256 value.
pub type Proof = [u8; PROOF_LEN];

/// Who gives a proof. The role is part of what is signed, so the service's proof cannot be sent
/// back to it as the bridge's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Bridge = 1,
    Service = 2,
}

/// The proof `role` gives for this handshake, keyed with `token`.
pub fn proof(token: &[u8], role: Role, bridge_nonce: &Nonce, service_nonce: &Nonce) -> Proof {
    let mut message = Vec::with_capacity(CONTEXT.len() + 1 + 2 * NONCE_LEN);
    message.extend_from_slice(CONTEXT);
    message.push(role as u8);
    message.extend_from_slice(bridge_nonce);
    message.extend_from_slice(service_nonce);
    hmac_sha256(token, &message)
}

/// Whether `given` is the proof `role` would give. The comparison takes the same time wherever
/// the two differ.
pub fn verify(
    token: &[u8],
    role: Role,
    bridge_nonce: &Nonce,
    service_nonce: &Nonce,
    given: &[u8],
) -> bool {
    let expected = proof(token, role, bridge_nonce, service_nonce);
    same(&expected, given)
}

/// Compares two byte strings without stopping at the first difference. Different lengths are
/// different, and the length is not a secret.
pub fn same(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut difference = 0u8;
    for (x, y) in a.iter().zip(b) {
        difference |= x ^ y;
    }
    std::hint::black_box(difference) == 0
}

/// HMAC-SHA-256 (RFC 2104). A key longer than the 64-byte block is hashed first.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut padded = [0u8; 64];
    if key.len() > padded.len() {
        padded[..32].copy_from_slice(&sha256(key));
    } else {
        padded[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(&padded.map(|byte| byte ^ 0x36));
    inner.update(message);
    let inner = inner.finish();
    let mut outer = Sha256::new();
    outer.update(&padded.map(|byte| byte ^ 0x5c));
    outer.update(&inner);
    outer.finish()
}

/// SHA-256 (FIPS 180-4).
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(data);
    hash.finish()
}

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

struct Sha256 {
    state: [u32; 8],
    block: [u8; 64],
    used: usize,
    total: u64,
}

impl Sha256 {
    fn new() -> Self {
        Sha256 {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            block: [0; 64],
            used: 0,
            total: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        while !data.is_empty() {
            let take = data.len().min(64 - self.used);
            self.block[self.used..self.used + take].copy_from_slice(&data[..take]);
            self.used += take;
            data = &data[take..];
            if self.used == 64 {
                let block = self.block;
                self.compress(&block);
                self.used = 0;
            }
        }
    }

    fn finish(mut self) -> [u8; 32] {
        let bits = self.total.wrapping_mul(8);
        self.update(&[0x80]);
        while self.used != 56 {
            self.update(&[0]);
        }
        self.update(&bits.to_be_bytes());
        let mut digest = [0u8; 32];
        for (i, word) in self.state.iter().enumerate() {
            digest[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        digest
    }

    fn compress(&mut self, chunk: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            let bytes = &chunk[i * 4..i * 4 + 4];
            *word = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for (&k, &word) in K.iter().zip(w.iter()) {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(choose)
                .wrapping_add(k)
                .wrapping_add(word);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (state, add) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *state = state.wrapping_add(add);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// 32 bytes counting up from `from`, as the specification's vectors use them.
    fn range(from: u8) -> [u8; 32] {
        std::array::from_fn(|i| from + i as u8)
    }

    #[test]
    fn sha256_matches_the_published_vectors() {
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // 56 bytes: the padding needs a second block.
        assert_eq!(
            hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        let million = vec![b'a'; 1_000_000];
        assert_eq!(
            hex(&sha256(&million)),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn hmac_matches_rfc_4231() {
        assert_eq!(
            hex(&hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // A key longer than the block is hashed first (test case 6).
        assert_eq!(
            hex(&hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn the_proofs_match_the_specification_vectors() {
        let token = range(0x00);
        let bridge_nonce = range(0x20);
        let service_nonce = range(0x40);
        assert_eq!(
            hex(&proof(&token, Role::Bridge, &bridge_nonce, &service_nonce)),
            "13d46f7dc5c37ff12cf336554acb4e1981ffbcc22ce374d68289bed0fd8f9651"
        );
        assert_eq!(
            hex(&proof(&token, Role::Service, &bridge_nonce, &service_nonce)),
            "f4ec03176babba0072aaf8625da4e41230e772613a0062121d5597db8cffbf69"
        );
        // A short token with bytes that are not text.
        let short = [0x73, 0x65, 0x63, 0x72, 0x65, 0x74, 0x01, 0xfe];
        assert_eq!(
            hex(&proof(&short, Role::Bridge, &bridge_nonce, &service_nonce)),
            "c561da91325fb75af59d36d89ff0057ffa6f832333ef35ce9719a3e991ec2fa3"
        );
        assert_eq!(
            hex(&proof(&short, Role::Service, &bridge_nonce, &service_nonce)),
            "1a6117546f912bebe685c274f3bfc080292dee261310c8941a6bd530567bd792"
        );
        // A token longer than the HMAC block.
        assert_eq!(
            hex(&proof(
                &[0xaa; 100],
                Role::Bridge,
                &bridge_nonce,
                &service_nonce
            )),
            "a68cfdba801506b73275bad33fdb8b28a8d36e4e7d5f14ca82178ea2593f122f"
        );
    }

    #[test]
    fn a_proof_does_not_verify_for_another_role_token_or_nonce() {
        let token = range(0x00);
        let (bridge_nonce, service_nonce) = (range(0x20), range(0x40));
        let given = proof(&token, Role::Bridge, &bridge_nonce, &service_nonce);
        assert!(verify(
            &token,
            Role::Bridge,
            &bridge_nonce,
            &service_nonce,
            &given
        ));
        // A reflected proof: the service's role is another message.
        assert!(!verify(
            &token,
            Role::Service,
            &bridge_nonce,
            &service_nonce,
            &given
        ));
        assert!(!verify(
            &range(0x01),
            Role::Bridge,
            &bridge_nonce,
            &service_nonce,
            &given
        ));
        assert!(!verify(
            &token,
            Role::Bridge,
            &service_nonce,
            &bridge_nonce,
            &given
        ));
        assert!(!verify(
            &token,
            Role::Bridge,
            &bridge_nonce,
            &service_nonce,
            &given[..31]
        ));
        let mut flipped = given;
        flipped[31] ^= 1;
        assert!(!verify(
            &token,
            Role::Bridge,
            &bridge_nonce,
            &service_nonce,
            &flipped
        ));
    }
}
