//! Frames between the AnyPS5 network bridge and the AnyPlay net service.
//!
//! The wire format is specified in `spec/bridge-protocol.md`. This crate is its reference; the
//! bridge in AnyPS5 implements the same file in C++.

use std::fmt;

/// The protocol version this crate speaks.
pub const VERSION: u16 = 0;

/// The largest payload a frame may carry.
pub const MAX_PAYLOAD: usize = 16 * 1024 * 1024;

const HEADER: usize = 12;

/// What a frame's payload is. Unknown kinds are kept, so either side can be newer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Kind(pub u16);

impl Kind {
    pub const HELLO: Kind = Kind(0x0001);
    pub const WELCOME: Kind = Kind(0x0002);
    pub const REJECT: Kind = Kind(0x0003);
}

/// One message on the connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub kind: Kind,
    /// 0 for an event; otherwise the number that pairs a request with its reply.
    pub request: u32,
    pub payload: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// A frame announced or carried more than [`MAX_PAYLOAD`] bytes. The connection must be closed.
    TooLarge(usize),
    /// A payload ended before all of its fields.
    Truncated,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::TooLarge(len) => write!(f, "frame payload of {len} bytes exceeds {MAX_PAYLOAD}"),
            Error::Truncated => f.write_str("payload ended before all of its fields"),
        }
    }
}

impl std::error::Error for Error {}

impl Frame {
    /// Appends the frame's bytes to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        let len = self.payload.len();
        if len > MAX_PAYLOAD {
            return Err(Error::TooLarge(len));
        }
        out.reserve(HEADER + len);
        out.extend_from_slice(&(len as u32).to_le_bytes());
        out.extend_from_slice(&self.kind.0.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&self.request.to_le_bytes());
        out.extend_from_slice(&self.payload);
        Ok(())
    }
}

/// Collects bytes as they arrive and cuts them into frames.
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete frame, or `None` until enough bytes have arrived.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, Error> {
        if self.buf.len() < HEADER {
            return Ok(None);
        }
        let len = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        if len > MAX_PAYLOAD {
            return Err(Error::TooLarge(len));
        }
        if self.buf.len() < HEADER + len {
            return Ok(None);
        }
        let kind = Kind(u16::from_le_bytes([self.buf[4], self.buf[5]]));
        let request = u32::from_le_bytes([self.buf[8], self.buf[9], self.buf[10], self.buf[11]]);
        let payload = self.buf[HEADER..HEADER + len].to_vec();
        self.buf.drain(..HEADER + len);
        Ok(Some(Frame {
            kind,
            request,
            payload,
        }))
    }
}

/// The bridge's first frame: the version it speaks and the secret of this game start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub version: u16,
    pub token: Vec<u8>,
}

impl Hello {
    pub fn frame(&self) -> Frame {
        let mut payload = self.version.to_le_bytes().to_vec();
        payload.extend_from_slice(&self.token);
        Frame {
            kind: Kind::HELLO,
            request: 0,
            payload,
        }
    }

    pub fn parse(payload: &[u8]) -> Result<Self, Error> {
        let (version, token) = split_version(payload)?;
        Ok(Self {
            version,
            token: token.to_vec(),
        })
    }
}

/// The net service's acceptance: the version both sides use from now on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Welcome {
    pub version: u16,
}

impl Welcome {
    pub fn frame(&self) -> Frame {
        Frame {
            kind: Kind::WELCOME,
            request: 0,
            payload: self.version.to_le_bytes().to_vec(),
        }
    }

    pub fn parse(payload: &[u8]) -> Result<Self, Error> {
        let (version, _) = split_version(payload)?;
        Ok(Self { version })
    }
}

/// The net service's refusal. The reason is for the log; the connection closes after it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reject {
    pub reason: String,
}

impl Reject {
    pub fn frame(&self) -> Frame {
        Frame {
            kind: Kind::REJECT,
            request: 0,
            payload: self.reason.as_bytes().to_vec(),
        }
    }

    pub fn parse(payload: &[u8]) -> Self {
        Self {
            reason: String::from_utf8_lossy(payload).into_owned(),
        }
    }
}

fn split_version(payload: &[u8]) -> Result<(u16, &[u8]), Error> {
    match payload {
        [lo, hi, rest @ ..] => Ok((u16::from_le_bytes([*lo, *hi]), rest)),
        _ => Err(Error::Truncated),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(frame: &Frame) -> Vec<u8> {
        let mut out = Vec::new();
        frame.encode(&mut out).unwrap();
        out
    }

    #[test]
    fn header_layout_matches_the_spec() {
        let frame = Frame {
            kind: Kind(0x0201),
            request: 7,
            payload: vec![0xAA, 0xBB],
        };
        assert_eq!(
            bytes(&frame),
            [2, 0, 0, 0, 0x01, 0x02, 0, 0, 7, 0, 0, 0, 0xAA, 0xBB]
        );
    }

    #[test]
    fn a_frame_survives_the_round_trip() {
        let frame = Hello {
            version: VERSION,
            token: b"secret".to_vec(),
        }
        .frame();
        let mut decoder = Decoder::new();
        decoder.push(&bytes(&frame));
        assert_eq!(decoder.next_frame(), Ok(Some(frame)));
        assert_eq!(decoder.next_frame(), Ok(None));
    }

    #[test]
    fn frames_arriving_byte_by_byte_come_out_whole() {
        let frame = Frame {
            kind: Kind::REJECT,
            request: 0,
            payload: b"wrong token".to_vec(),
        };
        let mut decoder = Decoder::new();
        let wire = bytes(&frame);
        for (i, byte) in wire.iter().enumerate() {
            decoder.push(&[*byte]);
            let got = decoder.next_frame().unwrap();
            if i + 1 < wire.len() {
                assert_eq!(got, None);
            } else {
                assert_eq!(got, Some(frame.clone()));
            }
        }
    }

    #[test]
    fn two_frames_in_one_read_come_out_in_order() {
        let first = Welcome { version: 0 }.frame();
        let second = Frame {
            kind: Kind(0x0302),
            request: 9,
            payload: Vec::new(),
        };
        let mut wire = bytes(&first);
        wire.extend(bytes(&second));
        let mut decoder = Decoder::new();
        decoder.push(&wire);
        assert_eq!(decoder.next_frame(), Ok(Some(first)));
        assert_eq!(decoder.next_frame(), Ok(Some(second)));
        assert_eq!(decoder.next_frame(), Ok(None));
    }

    #[test]
    fn an_oversized_length_is_refused_before_the_payload_arrives() {
        let mut header = ((MAX_PAYLOAD + 1) as u32).to_le_bytes().to_vec();
        header.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]);
        let mut decoder = Decoder::new();
        decoder.push(&header);
        assert_eq!(decoder.next_frame(), Err(Error::TooLarge(MAX_PAYLOAD + 1)));
    }

    #[test]
    fn an_oversized_payload_is_not_encoded() {
        let frame = Frame {
            kind: Kind(0x0200),
            request: 1,
            payload: vec![0; MAX_PAYLOAD + 1],
        };
        assert_eq!(
            frame.encode(&mut Vec::new()),
            Err(Error::TooLarge(MAX_PAYLOAD + 1))
        );
    }

    #[test]
    fn flags_are_ignored() {
        let mut wire = bytes(&Welcome { version: 0 }.frame());
        wire[6] = 0xFF;
        let mut decoder = Decoder::new();
        decoder.push(&wire);
        assert_eq!(
            decoder.next_frame(),
            Ok(Some(Welcome { version: 0 }.frame()))
        );
    }

    #[test]
    fn handshake_payloads_parse() {
        let hello = Hello {
            version: 3,
            token: vec![1, 2, 3],
        };
        assert_eq!(Hello::parse(&hello.frame().payload), Ok(hello));
        assert_eq!(Hello::parse(&[1]), Err(Error::Truncated));
        assert_eq!(Welcome::parse(&[0, 0]), Ok(Welcome { version: 0 }));
        assert_eq!(Welcome::parse(&[]), Err(Error::Truncated));
        assert_eq!(
            Reject::parse(b"version 9 unknown").reason,
            "version 9 unknown"
        );
    }
}
