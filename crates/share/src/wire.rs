//! The video and audio datagrams of `spec/share-play.md`, read and written. All numbers are
//! big-endian. A datagram that is read and written again comes out as the same bytes.

use std::fmt;

/// The wire version this module speaks.
pub const VERSION: u8 = 0;
pub const TYPE_VIDEO: u8 = 1;
pub const TYPE_AUDIO: u8 = 2;

/// Bytes in front of the payload of a video datagram.
pub const VIDEO_HEADER_LEN: usize = 20;
/// Bytes in front of the payload of an audio datagram.
pub const AUDIO_HEADER_LEN: usize = 10;

/// Groups in one frame, at most.
pub const MAX_GROUPS: u8 = 8;
/// Data packets in one group, at most.
pub const MAX_DATA: u16 = 254;
/// Parity packets in one group, at most.
pub const MAX_PARITY: u16 = 254;
/// Data and parity packets in one group together, at most.
pub const MAX_GROUP_PACKETS: u16 = 255;
/// The longest Opus packet (RFC 6716). The spec names no limit for the audio payload.
pub const MAX_AUDIO_PAYLOAD: usize = 1275;

const FLAG_KEY: u8 = 0x01;
const FLAG_PARITY: u8 = 0x02;
const VIDEO_FLAGS: u8 = FLAG_KEY | FLAG_PARITY;
const FLAG_INTERRUPTED: u8 = 0x01;

/// Why a datagram is refused. Each variant is one reason from the receiver rules of the spec; a
/// refused datagram gets no reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Shorter than the header and one byte of payload (21 bytes for video, 11 for audio).
    TooShort,
    /// Another version than 0.
    Version(u8),
    /// Not the type this reader reads.
    Type(u8),
    /// A flag bit set that must be 0.
    Flags(u8),
    /// `groups` is 0 or above 8.
    Groups(u8),
    /// `group` is not below `groups`.
    GroupIndex,
    /// `data` is 0 or above 254.
    DataCount(u16),
    /// `parity` is above 254.
    ParityCount(u16),
    /// `data + parity` is above 255.
    GroupSize,
    /// `index >= data + parity`.
    Index,
    /// The parity flag disagrees with the index (parity packets come after the data packets).
    ParityFlag,
    /// A payload of another length than its group's other packets. Needs the other packets to
    /// judge, so only the reassembler gives this.
    PayloadLength,
    /// An audio payload longer than an Opus packet can be.
    PayloadTooLong,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::TooShort => write!(f, "datagram shorter than its header"),
            Refusal::Version(v) => write!(f, "datagram version {v} is not supported"),
            Refusal::Type(t) => write!(f, "datagram type {t} is not the one expected"),
            Refusal::Flags(b) => write!(f, "datagram sets undefined flags {b:#04x}"),
            Refusal::Groups(g) => write!(f, "{g} groups in a frame"),
            Refusal::GroupIndex => write!(f, "group number is not below the number of groups"),
            Refusal::DataCount(d) => write!(f, "{d} data packets in a group"),
            Refusal::ParityCount(p) => write!(f, "{p} parity packets in a group"),
            Refusal::GroupSize => write!(f, "more than 255 packets in a group"),
            Refusal::Index => write!(f, "packet index outside its group"),
            Refusal::ParityFlag => write!(f, "parity flag does not match the packet index"),
            Refusal::PayloadLength => write!(f, "payload length differs from its group"),
            Refusal::PayloadTooLong => write!(f, "audio payload longer than an Opus packet"),
        }
    }
}

impl std::error::Error for Refusal {}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// One video datagram. `payload` borrows the bytes it was read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoDatagram<'a> {
    /// The frame is a key frame.
    pub key: bool,
    /// The packet is a parity packet (`index >= data`).
    pub is_parity: bool,
    /// Counts stream changes; a packet from another epoch than the current is dropped.
    pub epoch: u8,
    /// Counts the frames sent, wrapping.
    pub frame: u32,
    /// Host microseconds at capture, wrapping at 2^32.
    pub timestamp: u32,
    /// Which group of the frame.
    pub group: u8,
    /// Number of groups, 1 to 8.
    pub groups: u8,
    /// Place in the group: data packets first, then parity.
    pub index: u16,
    /// Data packets in the group, 1 to 254.
    pub data: u16,
    /// Parity packets in the group, 0 to 254, with `data + parity <= 255`.
    pub parity: u16,
    pub payload: &'a [u8],
}

impl<'a> VideoDatagram<'a> {
    /// Read a video datagram. Anything that is not exactly one is refused.
    pub fn parse(b: &'a [u8]) -> Result<VideoDatagram<'a>, Refusal> {
        if b.len() < VIDEO_HEADER_LEN + 1 {
            return Err(Refusal::TooShort);
        }
        if b[0] != VERSION {
            return Err(Refusal::Version(b[0]));
        }
        if b[1] != TYPE_VIDEO {
            return Err(Refusal::Type(b[1]));
        }
        if b[2] & !VIDEO_FLAGS != 0 {
            return Err(Refusal::Flags(b[2]));
        }
        let d = VideoDatagram {
            key: b[2] & FLAG_KEY != 0,
            is_parity: b[2] & FLAG_PARITY != 0,
            epoch: b[3],
            frame: be32(b, 4),
            timestamp: be32(b, 8),
            group: b[12],
            groups: b[13],
            index: be16(b, 14),
            data: be16(b, 16),
            parity: be16(b, 18),
            payload: &b[VIDEO_HEADER_LEN..],
        };
        d.validate()?;
        Ok(d)
    }

    /// The rules on the header fields, in the order the spec lists the reasons.
    pub fn validate(&self) -> Result<(), Refusal> {
        if self.payload.is_empty() {
            return Err(Refusal::TooShort);
        }
        if self.groups == 0 || self.groups > MAX_GROUPS {
            return Err(Refusal::Groups(self.groups));
        }
        if self.group >= self.groups {
            return Err(Refusal::GroupIndex);
        }
        if self.data == 0 || self.data > MAX_DATA {
            return Err(Refusal::DataCount(self.data));
        }
        if self.parity > MAX_PARITY {
            return Err(Refusal::ParityCount(self.parity));
        }
        if self.data + self.parity > MAX_GROUP_PACKETS {
            return Err(Refusal::GroupSize);
        }
        if self.index >= self.data + self.parity {
            return Err(Refusal::Index);
        }
        if self.is_parity != (self.index >= self.data) {
            return Err(Refusal::ParityFlag);
        }
        Ok(())
    }

    /// Write the datagram. Fields [`parse`](Self::parse) would refuse are refused here too.
    pub fn encode(&self) -> Result<Vec<u8>, Refusal> {
        self.validate()?;
        let mut out = Vec::with_capacity(VIDEO_HEADER_LEN + self.payload.len());
        out.push(VERSION);
        out.push(TYPE_VIDEO);
        out.push(
            if self.key { FLAG_KEY } else { 0 } | if self.is_parity { FLAG_PARITY } else { 0 },
        );
        out.push(self.epoch);
        out.extend_from_slice(&self.frame.to_be_bytes());
        out.extend_from_slice(&self.timestamp.to_be_bytes());
        out.push(self.group);
        out.push(self.groups);
        out.extend_from_slice(&self.index.to_be_bytes());
        out.extend_from_slice(&self.data.to_be_bytes());
        out.extend_from_slice(&self.parity.to_be_bytes());
        out.extend_from_slice(self.payload);
        Ok(out)
    }
}

/// One audio datagram: one Opus packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioDatagram<'a> {
    /// The stream was interrupted before this packet.
    pub interrupted: bool,
    pub epoch: u8,
    /// Counts audio packets sent, wrapping at 65536.
    pub sequence: u16,
    /// The sender's 48 kHz sample clock, wrapping.
    pub timestamp: u32,
    pub payload: &'a [u8],
}

impl<'a> AudioDatagram<'a> {
    pub fn parse(b: &'a [u8]) -> Result<AudioDatagram<'a>, Refusal> {
        if b.len() < AUDIO_HEADER_LEN + 1 {
            return Err(Refusal::TooShort);
        }
        if b[0] != VERSION {
            return Err(Refusal::Version(b[0]));
        }
        if b[1] != TYPE_AUDIO {
            return Err(Refusal::Type(b[1]));
        }
        if b[2] & !FLAG_INTERRUPTED != 0 {
            return Err(Refusal::Flags(b[2]));
        }
        let payload = &b[AUDIO_HEADER_LEN..];
        if payload.len() > MAX_AUDIO_PAYLOAD {
            return Err(Refusal::PayloadTooLong);
        }
        Ok(AudioDatagram {
            interrupted: b[2] & FLAG_INTERRUPTED != 0,
            epoch: b[3],
            sequence: be16(b, 4),
            timestamp: be32(b, 6),
            payload,
        })
    }

    pub fn encode(&self) -> Result<Vec<u8>, Refusal> {
        if self.payload.is_empty() {
            return Err(Refusal::TooShort);
        }
        if self.payload.len() > MAX_AUDIO_PAYLOAD {
            return Err(Refusal::PayloadTooLong);
        }
        let mut out = Vec::with_capacity(AUDIO_HEADER_LEN + self.payload.len());
        out.push(VERSION);
        out.push(TYPE_AUDIO);
        out.push(if self.interrupted {
            FLAG_INTERRUPTED
        } else {
            0
        });
        out.push(self.epoch);
        out.extend_from_slice(&self.sequence.to_be_bytes());
        out.extend_from_slice(&self.timestamp.to_be_bytes());
        out.extend_from_slice(self.payload);
        Ok(out)
    }
}

/// A datagram of either media type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Datagram<'a> {
    Video(VideoDatagram<'a>),
    Audio(AudioDatagram<'a>),
}

impl<'a> Datagram<'a> {
    /// Read a video or an audio datagram, by its type byte.
    pub fn parse(b: &'a [u8]) -> Result<Datagram<'a>, Refusal> {
        if b.len() < 2 {
            return Err(Refusal::TooShort);
        }
        if b[0] != VERSION {
            return Err(Refusal::Version(b[0]));
        }
        match b[1] {
            TYPE_VIDEO => VideoDatagram::parse(b).map(Datagram::Video),
            TYPE_AUDIO => AudioDatagram::parse(b).map(Datagram::Audio),
            t => Err(Refusal::Type(t)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video() -> Vec<u8> {
        VideoDatagram {
            key: true,
            is_parity: true,
            epoch: 3,
            frame: 0x0102_0304,
            timestamp: 0x0A0B_0C0D,
            group: 1,
            groups: 2,
            index: 5,
            data: 4,
            parity: 2,
            payload: &[0xAA, 0xBB],
        }
        .encode()
        .unwrap()
    }

    #[test]
    fn video_layout_is_the_one_in_the_spec() {
        assert_eq!(
            video(),
            [
                0, 1, 0x03, 3, 1, 2, 3, 4, 0x0A, 0x0B, 0x0C, 0x0D, 1, 2, 0, 5, 0, 4, 0, 2, 0xAA,
                0xBB
            ]
        );
    }

    #[test]
    fn video_round_trip() {
        let bytes = video();
        let d = VideoDatagram::parse(&bytes).unwrap();
        assert!(d.key && d.is_parity);
        assert_eq!(
            (d.epoch, d.frame, d.timestamp),
            (3, 0x0102_0304, 0x0A0B_0C0D)
        );
        assert_eq!(
            (d.group, d.groups, d.index, d.data, d.parity),
            (1, 2, 5, 4, 2)
        );
        assert_eq!(d.payload, &[0xAA, 0xBB]);
        assert_eq!(d.encode().unwrap(), bytes);
    }

    #[test]
    fn video_refusals() {
        let ok = video();
        let with = |at: usize, v: u8| {
            let mut b = ok.clone();
            b[at] = v;
            VideoDatagram::parse(&b).unwrap_err()
        };
        assert_eq!(
            VideoDatagram::parse(&ok[..20]).unwrap_err(),
            Refusal::TooShort
        );
        assert_eq!(VideoDatagram::parse(&[]).unwrap_err(), Refusal::TooShort);
        assert_eq!(with(0, 1), Refusal::Version(1));
        assert_eq!(with(1, 2), Refusal::Type(2));
        assert_eq!(with(2, 0x07), Refusal::Flags(0x07));
        assert_eq!(with(2, 0x80), Refusal::Flags(0x80));
        assert_eq!(with(13, 0), Refusal::Groups(0));
        assert_eq!(with(13, 9), Refusal::Groups(9));
        assert_eq!(with(12, 2), Refusal::GroupIndex);
        assert_eq!(with(17, 0), Refusal::DataCount(0));
        assert_eq!(with(16, 1), Refusal::DataCount(260));
        assert_eq!(with(18, 1), Refusal::ParityCount(258));
        // data 254 + parity 2 = 256
        let mut b = ok.clone();
        b[16] = 0;
        b[17] = 254;
        assert_eq!(VideoDatagram::parse(&b).unwrap_err(), Refusal::GroupSize);
        // index 6 = data + parity
        assert_eq!(with(15, 6), Refusal::Index);
        // index 3 is a data packet but the flag says parity
        assert_eq!(with(15, 3), Refusal::ParityFlag);
        // index 5 is parity; clear the flag
        assert_eq!(with(2, 0x01), Refusal::ParityFlag);
    }

    #[test]
    fn audio_round_trip_and_refusals() {
        let bytes = AudioDatagram {
            interrupted: true,
            epoch: 9,
            sequence: 0xFFFF,
            timestamp: 7,
            payload: &[1, 2, 3],
        }
        .encode()
        .unwrap();
        assert_eq!(bytes, [0, 2, 1, 9, 0xFF, 0xFF, 0, 0, 0, 7, 1, 2, 3]);
        let d = AudioDatagram::parse(&bytes).unwrap();
        assert!(d.interrupted);
        assert_eq!(d.encode().unwrap(), bytes);
        assert_eq!(
            AudioDatagram::parse(&bytes[..10]).unwrap_err(),
            Refusal::TooShort
        );
        let mut b = bytes.clone();
        b[2] = 2;
        assert_eq!(AudioDatagram::parse(&b).unwrap_err(), Refusal::Flags(2));
        let mut b = bytes.clone();
        b[0] = 4;
        assert_eq!(AudioDatagram::parse(&b).unwrap_err(), Refusal::Version(4));
        let mut b = bytes.clone();
        b[1] = 1;
        assert_eq!(AudioDatagram::parse(&b).unwrap_err(), Refusal::Type(1));
        let mut long = bytes[..AUDIO_HEADER_LEN].to_vec();
        long.extend(std::iter::repeat_n(0u8, MAX_AUDIO_PAYLOAD + 1));
        assert_eq!(
            AudioDatagram::parse(&long).unwrap_err(),
            Refusal::PayloadTooLong
        );
    }

    #[test]
    fn either_kind() {
        let v = video();
        assert!(matches!(Datagram::parse(&v), Ok(Datagram::Video(_))));
        let mut a = vec![0, 2, 0, 0, 0, 1, 0, 0, 0, 0, 5];
        assert!(matches!(Datagram::parse(&a), Ok(Datagram::Audio(_))));
        a[1] = 4;
        assert_eq!(Datagram::parse(&a).unwrap_err(), Refusal::Type(4));
        a[0] = 3;
        assert_eq!(Datagram::parse(&a).unwrap_err(), Refusal::Version(3));
    }
}
