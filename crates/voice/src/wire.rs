//! The voice frame on the wire, as `spec/voice.md` specifies it: eight header bytes and one Opus
//! packet. One frame is one datagram; nothing here knows about tunnels.

use std::fmt;

/// The wire version this module speaks.
pub const VERSION: u8 = 0;

/// Bytes in front of the Opus packet.
pub const HEADER_LEN: usize = 8;

/// The longest Opus packet a frame may carry. A frame then fits a 1200-byte datagram, the smallest
/// payload QUIC promises to carry whole.
pub const MAX_PAYLOAD: usize = 1200 - HEADER_LEN;

/// Set on the last frame of a stretch of talking.
const FLAG_END_OF_TALK: u8 = 0x01;
const KNOWN_FLAGS: u8 = FLAG_END_OF_TALK;

/// One voice frame. `payload` borrows the datagram it was read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame<'a> {
    /// Counts the frames the sender sent, wrapping at 65536. A gap means a frame was lost.
    pub sequence: u16,
    /// The sender's sample clock at 48 kHz when the frame's audio began, wrapping at 2^32. It
    /// advances by 960 per 20 ms whether or not a frame was sent, so a jump bigger than that
    /// between two consecutive sequence numbers is silence, not loss.
    pub timestamp: u32,
    /// This is the last frame of a stretch of talking; what follows is silence.
    pub end_of_talk: bool,
    /// One Opus packet of 20 ms, mono, 48 kHz.
    pub payload: &'a [u8],
}

/// Why bytes are not a voice frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireError {
    /// Fewer than [`HEADER_LEN`] bytes.
    TooShort,
    /// A version this module does not speak.
    Version(u8),
    /// A flag bit this version does not define.
    Flags(u8),
    /// A header and nothing behind it.
    EmptyPayload,
    /// More than [`MAX_PAYLOAD`] bytes behind the header.
    PayloadTooLong,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::TooShort => write!(f, "voice frame shorter than its header"),
            WireError::Version(v) => write!(f, "voice frame version {v} is not supported"),
            WireError::Flags(b) => write!(f, "voice frame sets undefined flags {b:#04x}"),
            WireError::EmptyPayload => write!(f, "voice frame without audio"),
            WireError::PayloadTooLong => write!(f, "voice frame longer than a datagram"),
        }
    }
}

impl std::error::Error for WireError {}

impl<'a> Frame<'a> {
    /// Read a frame. Everything that is not exactly a version 0 frame is refused.
    pub fn parse(bytes: &'a [u8]) -> Result<Frame<'a>, WireError> {
        if bytes.len() < HEADER_LEN {
            return Err(WireError::TooShort);
        }
        if bytes[0] != VERSION {
            return Err(WireError::Version(bytes[0]));
        }
        if bytes[1] & !KNOWN_FLAGS != 0 {
            return Err(WireError::Flags(bytes[1]));
        }
        let payload = &bytes[HEADER_LEN..];
        if payload.is_empty() {
            return Err(WireError::EmptyPayload);
        }
        if payload.len() > MAX_PAYLOAD {
            return Err(WireError::PayloadTooLong);
        }
        Ok(Frame {
            sequence: u16::from_be_bytes([bytes[2], bytes[3]]),
            timestamp: u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            end_of_talk: bytes[1] & FLAG_END_OF_TALK != 0,
            payload,
        })
    }

    /// Write the frame. A frame [`parse`](Frame::parse) would refuse is a programming error, so
    /// this panics on an empty or oversize payload.
    pub fn encode(&self) -> Vec<u8> {
        assert!(
            !self.payload.is_empty() && self.payload.len() <= MAX_PAYLOAD,
            "voice payload of {} bytes",
            self.payload.len()
        );
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        out.push(VERSION);
        out.push(if self.end_of_talk {
            FLAG_END_OF_TALK
        } else {
            0
        });
        out.extend_from_slice(&self.sequence.to_be_bytes());
        out.extend_from_slice(&self.timestamp.to_be_bytes());
        out.extend_from_slice(self.payload);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<u8> {
        Frame {
            sequence: 0x0102,
            timestamp: 0x0A0B_0C0D,
            end_of_talk: true,
            payload: &[0xAA, 0xBB],
        }
        .encode()
    }

    #[test]
    fn layout_is_the_one_in_the_spec() {
        assert_eq!(
            sample(),
            [0, 1, 0x01, 0x02, 0x0A, 0x0B, 0x0C, 0x0D, 0xAA, 0xBB]
        );
    }

    #[test]
    fn round_trip() {
        let bytes = sample();
        let frame = Frame::parse(&bytes).unwrap();
        assert_eq!(frame.sequence, 0x0102);
        assert_eq!(frame.timestamp, 0x0A0B_0C0D);
        assert!(frame.end_of_talk);
        assert_eq!(frame.payload, [0xAA, 0xBB]);
        assert_eq!(frame.encode(), bytes);
    }

    #[test]
    fn refuses_what_is_not_a_frame() {
        let good = sample();
        assert_eq!(Frame::parse(&[]), Err(WireError::TooShort));
        assert_eq!(Frame::parse(&good[..7]), Err(WireError::TooShort));
        assert_eq!(Frame::parse(&good[..8]), Err(WireError::EmptyPayload));

        let mut bad = good.clone();
        bad[0] = 1;
        assert_eq!(Frame::parse(&bad), Err(WireError::Version(1)));

        for bit in 1..8 {
            let mut bad = good.clone();
            bad[1] = 1 << bit;
            assert_eq!(Frame::parse(&bad), Err(WireError::Flags(1 << bit)));
        }

        let mut long = good[..HEADER_LEN].to_vec();
        long.resize(HEADER_LEN + MAX_PAYLOAD + 1, 7);
        assert_eq!(Frame::parse(&long), Err(WireError::PayloadTooLong));
        long.pop();
        assert!(Frame::parse(&long).is_ok());
    }

    #[test]
    fn end_of_talk_clear_reads_false() {
        let mut bytes = sample();
        bytes[1] = 0;
        assert!(!Frame::parse(&bytes).unwrap().end_of_talk);
    }

    /// Fixed-seed stand-in for the fuzz target: whatever parses writes back to the same bytes.
    #[test]
    fn parsed_frames_write_back_identically() {
        let mut x = 0x2545_F491u32;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        for _ in 0..20_000 {
            let len = (next() % 40) as usize;
            let mut bytes: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            if len > 1 && next() % 2 == 0 {
                bytes[0] = 0;
                bytes[1] &= 1;
            }
            // The same checks as the fuzz target `voice_frame`.
            match Frame::parse(&bytes) {
                Ok(frame) => {
                    assert_eq!(frame.encode(), bytes);
                    assert_eq!(frame.payload, &bytes[HEADER_LEN..]);
                }
                Err(WireError::TooShort) => assert!(bytes.len() < HEADER_LEN),
                Err(WireError::Version(v)) => assert!(bytes.len() >= HEADER_LEN && v != VERSION),
                Err(WireError::Flags(f)) => assert!(bytes[0] == VERSION && f & !1 != 0),
                Err(WireError::EmptyPayload) => assert_eq!(bytes.len(), HEADER_LEN),
                Err(WireError::PayloadTooLong) => unreachable!("inputs here are short"),
            }
        }
    }
}
