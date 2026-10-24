//! What a device proves to the cloud, and what a browser sign-in is checked with.
//!
//! * [`DeviceKey`]: an Ed25519 key that one installation makes at sign-in. The cloud keeps its
//!   public half with the device.
//! * A **proof** ([`make`], [`verify`]): a signature over one request and the credential it
//!   carries, sent in the `AnyPlay-Proof` header. A refresh token that was copied from a log or a
//!   proxy is useless without the key.
//! * [`pkce`]: the S256 challenge of RFC 7636 for the browser sign-in.
//! * [`loopback_redirect`]: the only kind of redirect a native client may ask for (RFC 8252).
//!
//! The format is specified in `spec/cloud-api.md` ("Device keys and proofs").

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};
use sha2::{Digest as _, Sha256};

/// The header that carries a proof.
pub const HEADER: &str = "AnyPlay-Proof";
/// How far a proof's time may differ from the verifier's clock, in seconds.
pub const WINDOW_SECS: u64 = 60;
/// The longest proof a verifier reads.
pub const MAX_PROOF_LEN: usize = 256;

const DOMAIN: &str = "anyplay-proof-v1";

/// A device's private key.
pub struct DeviceKey(SigningKey);

impl std::fmt::Debug for DeviceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DeviceKey(..)")
    }
}

impl DeviceKey {
    pub fn generate() -> Self {
        Self(SigningKey::from_bytes(&rand::random()))
    }

    /// Reads the secret as written by [`DeviceKey::secret`]: 32 bytes in unpadded URL-safe
    /// base64.
    pub fn from_secret(text: &str) -> Option<Self> {
        let bytes = URL_SAFE_NO_PAD.decode(text.trim()).ok()?;
        let secret: [u8; 32] = bytes.try_into().ok()?;
        Some(Self(SigningKey::from_bytes(&secret)))
    }

    /// The secret, for the credential store. Never log it.
    pub fn secret(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0.to_bytes())
    }

    /// The public key in the form the cloud takes: 32 bytes in unpadded URL-safe base64.
    pub fn public(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0.verifying_key().to_bytes())
    }
}

/// The request a proof is made for.
#[derive(Clone, Copy, Debug)]
pub struct Subject<'a> {
    /// `POST`, `GET`, ...
    pub method: &'a str,
    /// The path without query, such as `/v1/auth/refresh`.
    pub path: &'a str,
    /// The token the request carries: the refresh token, or the access token. Only its hash is
    /// signed, so a proof made for one token fits no other.
    pub credential: &'a str,
}

fn message(subject: &Subject<'_>, time: u64, nonce: &str) -> Vec<u8> {
    let digest = URL_SAFE_NO_PAD.encode(Sha256::digest(subject.credential.as_bytes()));
    format!(
        "{DOMAIN}\n{}\n{}\n{time}\n{nonce}\n{digest}",
        subject.method.to_ascii_uppercase(),
        subject.path
    )
    .into_bytes()
}

/// The header value `v1.<time>.<nonce>.<signature>` for a request, made at `now` with the given
/// 16-byte `nonce`. [`make`] draws the nonce itself.
pub fn make_with_nonce(
    key: &DeviceKey,
    subject: &Subject<'_>,
    now: u64,
    nonce: [u8; 16],
) -> String {
    let nonce = URL_SAFE_NO_PAD.encode(nonce);
    let signature = key.0.sign(&message(subject, now, &nonce));
    format!(
        "v1.{now}.{nonce}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    )
}

/// The header value for a request, with a random nonce.
pub fn make(key: &DeviceKey, subject: &Subject<'_>, now: u64) -> String {
    make_with_nonce(key, subject, now, rand::random())
}

/// Why a proof was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProofError {
    /// Not in the `v1.<time>.<nonce>.<signature>` shape.
    Malformed,
    /// Made too long ago or too far ahead ([`WINDOW_SECS`]).
    Time,
    /// The signature does not fit this key, request or credential.
    Signature,
}

impl std::fmt::Display for ProofError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ProofError::Malformed => "the proof is malformed",
            ProofError::Time => "the proof is too old or from the future",
            ProofError::Signature => "the proof does not fit this device, request or token",
        })
    }
}

impl std::error::Error for ProofError {}

/// A proof that checked out. Its nonce must be remembered until `expires` (Unix seconds), so the
/// same proof is not accepted twice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checked {
    pub nonce: String,
    pub expires: u64,
}

/// True when `public` is a device public key: 32 bytes of URL-safe base64 that are a point on
/// the curve.
pub fn public_key_ok(public: &str) -> bool {
    parse_public(public).is_some()
}

fn parse_public(public: &str) -> Option<VerifyingKey> {
    let bytes = URL_SAFE_NO_PAD.decode(public).ok()?;
    let bytes: [u8; 32] = bytes.try_into().ok()?;
    VerifyingKey::from_bytes(&bytes).ok()
}

/// Checks a proof against the device's public key, the request and the verifier's clock.
pub fn verify(
    public: &str,
    header: &str,
    subject: &Subject<'_>,
    now: u64,
) -> Result<Checked, ProofError> {
    if header.len() > MAX_PROOF_LEN {
        return Err(ProofError::Malformed);
    }
    let rest = header.strip_prefix("v1.").ok_or(ProofError::Malformed)?;
    let mut parts = rest.split('.');
    let (Some(time), Some(nonce), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(ProofError::Malformed);
    };
    let time: u64 = time.parse().map_err(|_| ProofError::Malformed)?;
    if nonce.len() != 22 || URL_SAFE_NO_PAD.decode(nonce).is_err() {
        return Err(ProofError::Malformed);
    }
    let signature = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|_| ProofError::Malformed)?;
    let signature: [u8; 64] = signature.try_into().map_err(|_| ProofError::Malformed)?;
    let key = parse_public(public).ok_or(ProofError::Signature)?;
    key.verify_strict(
        &message(subject, time, nonce),
        &Signature::from_bytes(&signature),
    )
    .map_err(|_| ProofError::Signature)?;
    if time.abs_diff(now) > WINDOW_SECS {
        return Err(ProofError::Time);
    }
    Ok(Checked {
        nonce: nonce.to_owned(),
        expires: time + WINDOW_SECS + 1,
    })
}

/// PKCE (RFC 7636) with the S256 method, the only one we take.
pub mod pkce {
    use super::*;
    use subtle::ConstantTimeEq as _;

    /// A new verifier: 43 characters, 256 bits.
    pub fn new_verifier() -> String {
        let bytes: [u8; 32] = rand::random();
        URL_SAFE_NO_PAD.encode(bytes)
    }

    /// A random `state` value for the redirect.
    pub fn new_state() -> String {
        let bytes: [u8; 16] = rand::random();
        URL_SAFE_NO_PAD.encode(bytes)
    }

    /// True for 43 to 128 characters of `A-Z a-z 0-9 - . _ ~`.
    pub fn verifier_ok(verifier: &str) -> bool {
        (43..=128).contains(&verifier.len())
            && verifier
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
    }

    /// `BASE64URL(SHA256(verifier))`.
    pub fn challenge(verifier: &str) -> String {
        URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
    }

    /// True for the 43 URL-safe characters of an unpadded SHA-256 digest.
    pub fn challenge_ok(challenge: &str) -> bool {
        challenge.len() == 43
            && challenge
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    }

    /// True when `verifier` belongs to `challenge`.
    pub fn matches(verifier: &str, challenge: &str) -> bool {
        verifier_ok(verifier)
            && self::challenge(verifier)
                .as_bytes()
                .ct_eq(challenge.as_bytes())
                .into()
    }
}

/// Checks a `redirect_uri` a native client gave and returns it unchanged when it is a loopback
/// address as RFC 8252 section 7.3 describes: `http`, the IP literal `127.0.0.1` or `[::1]`, a
/// port from 1024 up, a plain path, and no user information, query or fragment. `localhost`
/// and every other host are refused, so a code can never be sent to somebody else.
pub fn loopback_redirect(uri: &str) -> Option<&str> {
    if uri.len() > 200 {
        return None;
    }
    let rest = uri.strip_prefix("http://")?;
    let (authority, path) = rest.split_once('/')?;
    let (host, port_text) = authority.rsplit_once(':')?;
    if host != "127.0.0.1" && host != "[::1]" {
        return None;
    }
    let port: u32 = port_text.parse().ok()?;
    if !(1024..=65535).contains(&port) || port.to_string() != port_text {
        return None;
    }
    let path_ok = path
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-._~/".contains(&b));
    (path_ok && !path.contains("..") && !path.contains("//")).then_some(uri)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    fn key() -> DeviceKey {
        DeviceKey::from_secret(&URL_SAFE_NO_PAD.encode([7u8; 32])).unwrap()
    }

    fn subject<'a>() -> Subject<'a> {
        Subject {
            method: "POST",
            path: "/v1/auth/refresh",
            credential: "refresh-token-1",
        }
    }

    /// A fixed key, request, time and nonce give a fixed proof. Another implementation of the
    /// format must produce this exact string.
    #[test]
    fn test_vector() {
        let proof = make_with_nonce(&key(), &subject(), NOW, [1u8; 16]);
        assert_eq!(
            key().public(),
            "6kpsY-KcUgq-9VB7Ey7F-ZVHdq6-vnuSQh7qaRRG0iw"
        );
        assert_eq!(
            proof,
            "v1.1800000000.AQEBAQEBAQEBAQEBAQEBAQ.nyoO-XK2j-igBkE6cd81u4D3dW5O4JKKa62lLTRZU4LOfJj6Vt8Gopf6TGpJBqneapSZxWgs9x7C26rXSQxuAQ"
        );
    }

    #[test]
    fn a_proof_checks_out_once_and_names_its_nonce() {
        let proof = make(&key(), &subject(), NOW);
        let checked = verify(&key().public(), &proof, &subject(), NOW + 5).unwrap();
        assert_eq!(checked.nonce.len(), 22);
        assert_eq!(checked.expires, NOW + WINDOW_SECS + 1);
    }

    #[test]
    fn another_key_is_refused() {
        let proof = make(&DeviceKey::generate(), &subject(), NOW);
        assert_eq!(
            verify(&key().public(), &proof, &subject(), NOW),
            Err(ProofError::Signature)
        );
    }

    #[test]
    fn another_token_path_or_method_is_refused() {
        let proof = make(&key(), &subject(), NOW);
        let public = key().public();
        for other in [
            Subject {
                credential: "refresh-token-2",
                ..subject()
            },
            Subject {
                path: "/v1/auth/signout",
                ..subject()
            },
            Subject {
                method: "GET",
                ..subject()
            },
        ] {
            assert_eq!(
                verify(&public, &proof, &other, NOW),
                Err(ProofError::Signature)
            );
        }
    }

    #[test]
    fn the_time_window_is_a_minute_either_way() {
        let proof = make(&key(), &subject(), NOW);
        let public = key().public();
        assert!(verify(&public, &proof, &subject(), NOW + WINDOW_SECS).is_ok());
        assert!(verify(&public, &proof, &subject(), NOW - WINDOW_SECS).is_ok());
        assert_eq!(
            verify(&public, &proof, &subject(), NOW + WINDOW_SECS + 1),
            Err(ProofError::Time)
        );
        assert_eq!(
            verify(&public, &proof, &subject(), NOW - WINDOW_SECS - 1),
            Err(ProofError::Time)
        );
    }

    #[test]
    fn a_changed_time_breaks_the_signature() {
        let proof = make(&key(), &subject(), NOW);
        let public = key().public();
        let mut parts: Vec<&str> = proof.split('.').collect();
        parts[1] = "1800000001";
        assert_eq!(
            verify(&public, &parts.join("."), &subject(), NOW),
            Err(ProofError::Signature)
        );
    }

    #[test]
    fn garbage_is_malformed() {
        let public = key().public();
        for text in [
            "",
            "v1.",
            "v2.1.2.3",
            "v1.x.AQEBAQEBAQEBAQEBAQEBAQ.AAAA",
            "v1.1800000000.short.AAAA",
            "v1.1800000000.AQEBAQEBAQEBAQEBAQEBAQ.AAAA",
            "v1.1.2.3.4",
        ] {
            assert_eq!(
                verify(&public, text, &subject(), NOW),
                Err(ProofError::Malformed),
                "{text:?}"
            );
        }
        let long = "a".repeat(MAX_PROOF_LEN + 1);
        assert_eq!(
            verify(&public, &long, &subject(), NOW),
            Err(ProofError::Malformed)
        );
    }

    #[test]
    fn keys_round_trip_through_text() {
        let key = DeviceKey::generate();
        let again = DeviceKey::from_secret(&key.secret()).unwrap();
        assert_eq!(key.public(), again.public());
        assert!(public_key_ok(&key.public()));
        assert!(!public_key_ok("AAAA"));
        assert!(!public_key_ok(""));
        assert!(DeviceKey::from_secret("nonsense").is_none());
    }

    /// RFC 7636, appendix B.
    #[test]
    fn pkce_follows_the_rfc() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(pkce::challenge(verifier), challenge);
        assert!(pkce::matches(verifier, challenge));
        assert!(!pkce::matches(&"a".repeat(43), challenge));
        assert!(!pkce::matches("short", &pkce::challenge("short")));
        assert!(pkce::challenge_ok(challenge));
        assert!(!pkce::challenge_ok("short"));
        let made = pkce::new_verifier();
        assert!(pkce::verifier_ok(&made));
        assert_ne!(made, pkce::new_verifier());
    }

    #[test]
    fn only_loopback_redirects_pass() {
        for ok in [
            "http://127.0.0.1:52345/callback",
            "http://127.0.0.1:1024/cb",
            "http://[::1]:49152/callback",
        ] {
            assert_eq!(loopback_redirect(ok), Some(ok), "{ok}");
        }
        for bad in [
            "https://127.0.0.1:52345/callback",
            "http://localhost:52345/callback",
            "http://127.0.0.2:52345/callback",
            "http://127.0.0.1/callback",
            "http://127.0.0.1:80/callback",
            "http://127.0.0.1:0/callback",
            "http://127.0.0.1:99999/callback",
            "http://127.0.0.1:052345/callback",
            "http://127.0.0.1:52345",
            "http://127.0.0.1:52345/callback?x=1",
            "http://127.0.0.1:52345/callback#x",
            "http://127.0.0.1:52345/../x",
            "http://user@127.0.0.1:52345/callback",
            "http://127.0.0.1:52345@evil.example/callback",
            "http://evil.example/callback",
            "http://127.0.0.1.evil.example:52345/callback",
            "https://evil.example/",
            "anyplay://callback",
            "",
        ] {
            assert_eq!(loopback_redirect(bad), None, "{bad}");
        }
    }
}
