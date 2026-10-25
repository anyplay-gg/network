//! Cuts one encoded frame into video datagrams with Reed-Solomon parity, as the section
//! "Packetising and loss protection" of `spec/share-play.md` says.
//!
//! The spec does not say how the receiver learns the frame's exact length when the last data
//! packet is short and gets lost. This packetiser puts the length of the encoded frame as four
//! big-endian bytes in front of the frame, so it sits at the start of the first data packet
//! (which parity can rebuild like any other). Only the last data packet of a frame is short.

use std::fmt;

use crate::codec::EncodedFrame;
use crate::rs;
use crate::wire::{MAX_DATA, MAX_GROUP_PACKETS, MAX_GROUPS, VIDEO_HEADER_LEN, VideoDatagram};

/// The smallest payload size a caller may ask for (`max_datagram_size - 20`, at least 1000).
pub const MIN_PAYLOAD: usize = 1000;

/// Bytes in front of the frame inside the packet stream.
pub(crate) const LENGTH_PREFIX: usize = 4;

/// A key frame gets at least this many percent of parity when parity is on at all.
pub const KEY_FRAME_MIN_PARITY_PERCENT: u8 = 10;

/// Why a frame was not packetised.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketiseError {
    /// The frame does not fit eight groups of at most 255 packets with this much parity.
    FrameTooLarge { len: usize },
}

impl fmt::Display for PacketiseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PacketiseError::FrameTooLarge { len } => {
                write!(f, "a frame of {len} bytes does not fit eight groups")
            }
        }
    }
}

impl std::error::Error for PacketiseError {}

/// Parity packets for `data` data packets at `percent`: `ceil(data * percent / 100)`.
pub fn parity_for(data: usize, percent: u8) -> usize {
    (data * percent as usize).div_ceil(100)
}

/// The sending half: numbers frames and cuts them into datagrams.
#[derive(Clone, Debug)]
pub struct Packetiser {
    max_payload: usize,
    epoch: u8,
    next_frame: u32,
}

impl Packetiser {
    /// `max_payload` is the connection's `max_datagram_size - 20`; it is raised to
    /// [`MIN_PAYLOAD`] if smaller.
    pub fn new(max_payload: usize, epoch: u8) -> Packetiser {
        Packetiser {
            max_payload: max_payload.max(MIN_PAYLOAD),
            epoch,
            next_frame: 0,
        }
    }

    /// The connection's datagram size changed.
    pub fn set_max_payload(&mut self, max_payload: usize) {
        self.max_payload = max_payload.max(MIN_PAYLOAD);
    }

    /// A new stream setup (codec or size): later datagrams carry this epoch.
    pub fn set_epoch(&mut self, epoch: u8) {
        self.epoch = epoch;
    }

    pub fn epoch(&self) -> u8 {
        self.epoch
    }

    /// Continue numbering from `n` (a sender that resumes, and the tests).
    pub fn set_next_frame(&mut self, n: u32) {
        self.next_frame = n;
    }

    /// The number the next frame gets.
    pub fn next_frame_number(&self) -> u32 {
        self.next_frame
    }

    /// Cut `frame` into datagrams, data packets first, then parity. `parity_percent` is the
    /// rate controller's `r`; a key frame gets at least 10% when it is above 0. A frame that is
    /// refused does not use up a frame number.
    pub fn packetise(
        &mut self,
        frame: &EncodedFrame,
        parity_percent: u8,
    ) -> Result<Vec<Vec<u8>>, PacketiseError> {
        let percent = if frame.key && parity_percent > 0 {
            parity_percent.max(KEY_FRAME_MIN_PARITY_PERCENT)
        } else {
            parity_percent
        };
        let too_large = PacketiseError::FrameTooLarge {
            len: frame.data.len(),
        };
        let total = frame
            .data
            .len()
            .checked_add(LENGTH_PREFIX)
            .ok_or(too_large)?;
        let len = u32::try_from(frame.data.len()).map_err(|_| too_large)?;
        let l = self.max_payload;
        let n = total.div_ceil(l);

        // The fewest groups in which every group fits 255 packets with its parity.
        let groups = (1..=(MAX_GROUPS as usize).min(n))
            .find(|&g| {
                let d = n.div_ceil(g);
                d <= MAX_DATA as usize && d + parity_for(d, percent) <= MAX_GROUP_PACKETS as usize
            })
            .ok_or(too_large)?;

        let mut stream = Vec::with_capacity(total);
        stream.extend_from_slice(&len.to_be_bytes());
        stream.extend_from_slice(&frame.data);

        let frame_no = self.next_frame;
        self.next_frame = self.next_frame.wrapping_add(1);

        let base = n / groups;
        let extra = n % groups;
        let mut out = Vec::new();
        let mut offset = 0usize;
        for g in 0..groups {
            let d = base + usize::from(g < extra);
            let p = parity_for(d, percent);
            let mut shards: Vec<Vec<u8>> = Vec::with_capacity(d);
            for _ in 0..d {
                let end = (offset + l).min(total);
                shards.push(stream[offset..end].to_vec());
                offset = end;
            }
            // Parity has the length of the longest data packet; shorter ones count as padded.
            let longest = shards.iter().map(Vec::len).max().unwrap_or(0);
            let mut padded = shards.clone();
            for s in &mut padded {
                s.resize(longest, 0);
            }
            let parity = rs::encode(&padded, p);

            let header = |index: usize, payload: &[u8]| -> Vec<u8> {
                VideoDatagram {
                    key: frame.key,
                    is_parity: index >= d,
                    epoch: self.epoch,
                    frame: frame_no,
                    timestamp: frame.timestamp_us,
                    group: g as u8,
                    groups: groups as u8,
                    index: index as u16,
                    data: d as u16,
                    parity: p as u16,
                    payload,
                }
                .encode()
                .expect("header fields are valid by construction")
            };
            for (i, s) in shards.iter().enumerate() {
                out.push(header(i, s));
            }
            for (i, s) in parity.iter().enumerate() {
                out.push(header(d + i, s));
            }
        }
        debug_assert_eq!(offset, total);
        debug_assert!(out.iter().all(|d| d.len() <= VIDEO_HEADER_LEN + l));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(len: usize, key: bool) -> EncodedFrame {
        EncodedFrame {
            key,
            timestamp_us: 123_456,
            data: (0..len).map(|i| (i * 7 + 3) as u8).collect(),
        }
    }

    fn parse(d: &[Vec<u8>]) -> Vec<VideoDatagram<'_>> {
        d.iter().map(|b| VideoDatagram::parse(b).unwrap()).collect()
    }

    #[test]
    fn small_frame_is_one_packet_without_parity() {
        let mut p = Packetiser::new(1180, 4);
        let out = p.packetise(&frame(100, false), 0).unwrap();
        assert_eq!(out.len(), 1);
        let d = VideoDatagram::parse(&out[0]).unwrap();
        assert_eq!((d.epoch, d.frame, d.timestamp), (4, 0, 123_456));
        assert_eq!(
            (d.group, d.groups, d.index, d.data, d.parity),
            (0, 1, 0, 1, 0)
        );
        assert!(!d.key && !d.is_parity);
        assert_eq!(&d.payload[..4], &100u32.to_be_bytes());
        assert_eq!(out[0].len(), VIDEO_HEADER_LEN + 104);
        assert_eq!(p.next_frame_number(), 1);
    }

    #[test]
    fn cuts_at_max_payload_and_adds_ceil_parity() {
        let mut p = Packetiser::new(1000, 0);
        // 5000 + 4 bytes = 6 packets, the last one short
        let out = p.packetise(&frame(5000, false), 20).unwrap();
        let ds = parse(&out);
        let data: Vec<_> = ds.iter().filter(|d| !d.is_parity).collect();
        let parity: Vec<_> = ds.iter().filter(|d| d.is_parity).collect();
        assert_eq!(data.len(), 6);
        assert_eq!(parity.len(), 2); // ceil(6 * 0.2) = 2
        assert!(data[..5].iter().all(|d| d.payload.len() == 1000));
        assert_eq!(data[5].payload.len(), 5004 - 5000);
        assert!(parity.iter().all(|d| d.payload.len() == 1000));
        assert!(ds.iter().all(|d| d.data == 6 && d.parity == 2));
        // data first, then parity
        assert!(ds[..6].iter().all(|d| !d.is_parity));
    }

    #[test]
    fn key_frames_get_at_least_ten_percent() {
        let mut p = Packetiser::new(1000, 0);
        let k = p.packetise(&frame(19_000, true), 5).unwrap(); // 20 packets
        assert_eq!(parse(&k).iter().filter(|d| d.is_parity).count(), 2);
        let n = p.packetise(&frame(19_000, false), 5).unwrap();
        assert_eq!(parse(&n).iter().filter(|d| d.is_parity).count(), 1);
        let off = p.packetise(&frame(19_000, true), 0).unwrap();
        assert_eq!(parse(&off).iter().filter(|d| d.is_parity).count(), 0);
        assert!(parse(&k).iter().all(|d| d.key));
    }

    #[test]
    fn big_frames_split_into_equal_groups() {
        let mut p = Packetiser::new(1000, 0);
        // 300 packets: one group would need 300 data packets
        let out = p.packetise(&frame(299_996, false), 0).unwrap();
        let ds = parse(&out);
        assert!(ds.iter().all(|d| d.groups == 2));
        assert_eq!(ds.iter().filter(|d| d.group == 0).count(), 150);
        assert_eq!(ds.iter().filter(|d| d.group == 1).count(), 150);
        // with 30% parity a group holds at most 196 data packets
        let out = p.packetise(&frame(299_996, false), 30).unwrap();
        let ds = parse(&out);
        assert!(ds.iter().all(|d| d.groups == 2));
        assert!(
            ds.iter()
                .all(|d| d.data as usize + d.parity as usize <= 255)
        );
        assert_eq!(ds.iter().filter(|d| !d.is_parity).count(), 300);
    }

    #[test]
    fn a_frame_too_big_for_eight_groups_is_refused() {
        let mut p = Packetiser::new(1000, 0);
        let max = 8 * 254 * 1000 - LENGTH_PREFIX;
        assert!(p.packetise(&frame(max, false), 0).is_ok());
        let err = p.packetise(&frame(max + 1, false), 0).unwrap_err();
        assert_eq!(err, PacketiseError::FrameTooLarge { len: max + 1 });
        // parity makes the limit smaller
        assert!(p.packetise(&frame(max, false), 30).is_err());
        // a refused frame does not use a number
        assert_eq!(p.next_frame_number(), 1);
    }

    #[test]
    fn numbering_epoch_and_wrap() {
        let mut p = Packetiser::new(5000, 1);
        p.set_next_frame(u32::MAX);
        let a = p.packetise(&frame(10, true), 0).unwrap();
        p.set_epoch(2);
        let b = p.packetise(&frame(10, false), 0).unwrap();
        let (a, b) = (parse(&a)[0], parse(&b)[0]);
        assert_eq!((a.frame, a.epoch), (u32::MAX, 1));
        assert_eq!((b.frame, b.epoch), (0, 2));
        // a max payload below 1000 is raised
        assert_eq!(Packetiser::new(10, 0).max_payload, MIN_PAYLOAD);
    }
}
