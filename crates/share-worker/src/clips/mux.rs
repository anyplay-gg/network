//! Writes the pictures of a [`Ring`](super::ring::Ring) as an MP4 file: one H.264 track, no
//! sound yet. The encoder's Annex B is cut into NAL units and written length-prefixed (AVCC),
//! the SPS and PPS go into the file's `avcC` box once.

use std::io::{Seek, Write};

use mp4::{AvcConfig, MediaConfig, Mp4Config, Mp4Sample, Mp4Writer, TrackConfig, TrackType};

use super::ring::ClipFrame;

/// Samples are timed in 1/90000 s, the usual clock of H.264 in MP4.
const TIMESCALE: u32 = 90_000;

/// Why a clip could not be written.
#[derive(Debug)]
pub enum MuxError {
    /// Nothing to write, or the first picture is not a key frame.
    NoKeyFrame,
    /// The first key frame carries no SPS and PPS.
    NoParameterSets,
    /// The picture is larger than a 16-bit MP4 size can hold.
    TooLarge,
    Mp4(mp4::Error),
    Io(std::io::Error),
}

impl std::fmt::Display for MuxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MuxError::NoKeyFrame => write!(f, "there is no key frame to start a clip with"),
            MuxError::NoParameterSets => write!(f, "the first key frame has no SPS and PPS"),
            MuxError::TooLarge => write!(f, "the picture is too large for an MP4 file"),
            MuxError::Mp4(e) => write!(f, "{e}"),
            MuxError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for MuxError {}

impl From<mp4::Error> for MuxError {
    fn from(e: mp4::Error) -> Self {
        MuxError::Mp4(e)
    }
}

impl From<std::io::Error> for MuxError {
    fn from(e: std::io::Error) -> Self {
        MuxError::Io(e)
    }
}

/// The NAL units of an Annex B stream (start codes `00 00 01` or `00 00 00 01`), without the
/// start codes.
pub fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut units = Vec::with_capacity(starts.len());
    for (n, &start) in starts.iter().enumerate() {
        let mut end = starts.get(n + 1).map_or(data.len(), |next| next - 3);
        // The zero before a four-byte start code, and trailing zeros, belong to no unit.
        while end > start && data[end - 1] == 0 {
            end -= 1;
        }
        if end > start {
            units.push(&data[start..end]);
        }
    }
    units
}

fn nal_type(unit: &[u8]) -> u8 {
    unit.first().map_or(0, |b| b & 0x1f)
}

/// The picture's slices and SEI as length-prefixed units. SPS, PPS and access unit delimiters
/// stay out: the first two are in the file's header and the last means nothing in MP4.
fn avcc(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 16);
    for unit in nal_units(data) {
        if matches!(nal_type(unit), 7..=9) {
            continue;
        }
        out.extend_from_slice(&(unit.len() as u32).to_be_bytes());
        out.extend_from_slice(unit);
    }
    out
}

/// Writes `frames` (oldest first, starting with a key frame) as an MP4 file of `width` by
/// `height`. The last picture lasts as long as the one before it.
pub fn write_mp4<W: Write + Seek>(
    out: W,
    frames: &[ClipFrame],
    width: u32,
    height: u32,
) -> Result<(), MuxError> {
    let first = frames
        .first()
        .filter(|f| f.key)
        .ok_or(MuxError::NoKeyFrame)?;
    let units = nal_units(&first.data);
    let sps = units.iter().find(|u| nal_type(u) == 7);
    let pps = units.iter().find(|u| nal_type(u) == 8);
    let (Some(sps), Some(pps)) = (sps, pps) else {
        return Err(MuxError::NoParameterSets);
    };
    let (width, height) = (
        u16::try_from(width).map_err(|_| MuxError::TooLarge)?,
        u16::try_from(height).map_err(|_| MuxError::TooLarge)?,
    );
    let config = Mp4Config {
        major_brand: str::parse("isom").expect("a four character code"),
        minor_version: 512,
        compatible_brands: ["isom", "iso2", "avc1", "mp41"]
            .iter()
            .map(|b| str::parse(b).expect("a four character code"))
            .collect(),
        timescale: 1000,
    };
    let mut writer = Mp4Writer::write_start(out, &config)?;
    writer.add_track(&TrackConfig {
        track_type: TrackType::Video,
        timescale: TIMESCALE,
        language: "und".to_owned(),
        media_conf: MediaConfig::AvcConfig(AvcConfig {
            width,
            height,
            seq_param_set: sps.to_vec(),
            pic_param_set: pps.to_vec(),
        }),
    })?;
    let to_ticks = |us: u64| us * u64::from(TIMESCALE) / 1_000_000;
    let origin = first.pts_us;
    let mut last_duration = 0;
    for (n, frame) in frames.iter().enumerate() {
        let start = to_ticks(frame.pts_us.saturating_sub(origin));
        let duration = match frames.get(n + 1) {
            Some(next) => to_ticks(next.pts_us.saturating_sub(origin)).saturating_sub(start) as u32,
            None => last_duration,
        };
        last_duration = duration;
        writer.write_sample(
            1,
            &Mp4Sample {
                start_time: start,
                duration,
                rendering_offset: 0,
                is_sync: frame.key,
                bytes: avcc(&frame.data).into(),
            },
        )?;
    }
    writer.write_end()?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Cursor;

    pub const SPS: [u8; 9] = [0x67, 0x42, 0xc0, 0x1e, 0xd9, 0x00, 0xa0, 0x47, 0xfe];
    pub const PPS: [u8; 4] = [0x68, 0xce, 0x3c, 0x80];

    /// A picture shaped like the encoder's: parameter sets before a key frame's slice.
    pub fn picture(key: bool, payload: u8) -> Vec<u8> {
        let mut out = Vec::new();
        if key {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&SPS);
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&PPS);
            out.extend_from_slice(&[0, 0, 1, 0x65, payload, payload, 0x11]);
        } else {
            out.extend_from_slice(&[0, 0, 0, 1, 0x41, payload, payload, 0x22]);
        }
        out
    }

    fn frames(count: u64) -> Vec<ClipFrame> {
        (0..count)
            .map(|n| ClipFrame {
                key: n % 10 == 0,
                pts_us: n * 33_333,
                data: picture(n % 10 == 0, n as u8).into(),
            })
            .collect()
    }

    #[test]
    fn annex_b_is_cut_at_both_kinds_of_start_code() {
        let data = picture(true, 7);
        let units = nal_units(&data);
        assert_eq!(units.len(), 3);
        assert_eq!(units[0], SPS);
        assert_eq!(units[1], PPS);
        assert_eq!(units[2], [0x65, 7, 7, 0x11]);
    }

    #[test]
    fn the_file_reads_back_with_every_picture_and_the_right_flags() {
        let frames = frames(35);
        let mut file = Cursor::new(Vec::new());
        write_mp4(&mut file, &frames, 1280, 720).expect("writes");
        let bytes = file.into_inner();
        let size = bytes.len() as u64;
        let mut reader =
            mp4::Mp4Reader::read_header(Cursor::new(bytes), size).expect("a valid MP4 file");
        let track = reader.tracks().get(&1).expect("one track");
        assert_eq!(track.width(), 1280);
        assert_eq!(track.height(), 720);
        assert_eq!(track.sample_count(), 35);
        let duration = reader.duration().as_secs_f64();
        assert!((duration - 35.0 / 30.0).abs() < 0.05, "{duration}");
        let first = reader.read_sample(1, 1).expect("read").expect("sample");
        assert!(first.is_sync);
        let second = reader.read_sample(1, 2).expect("read").expect("sample");
        assert!(!second.is_sync);
        // Length-prefixed, without the parameter sets.
        assert_eq!(&first.bytes[..], [0, 0, 0, 4, 0x65, 0, 0, 0x11]);
        assert_eq!(&second.bytes[..], [0, 0, 0, 4, 0x41, 1, 1, 0x22]);
    }

    #[test]
    fn a_clip_must_start_on_a_key_frame_with_parameter_sets() {
        let mut delta = frames(3);
        delta.remove(0);
        let mut out = Cursor::new(Vec::new());
        assert!(matches!(
            write_mp4(&mut out, &delta, 64, 64),
            Err(MuxError::NoKeyFrame)
        ));
        assert!(matches!(
            write_mp4(&mut out, &[], 64, 64),
            Err(MuxError::NoKeyFrame)
        ));
        let bare = ClipFrame {
            key: true,
            pts_us: 0,
            data: vec![0, 0, 0, 1, 0x65, 1, 2].into(),
        };
        assert!(matches!(
            write_mp4(&mut out, &[bare], 64, 64),
            Err(MuxError::NoParameterSets)
        ));
    }
}
