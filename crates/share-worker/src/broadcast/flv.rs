//! The bodies of RTMP video and audio messages: FLV tags without their headers.
//!
//! Video is H.264 in AVCC form (length-prefixed units) with the decoder configuration sent
//! first; audio is AAC-LC, 48 kHz stereo, and for now only silence.

use crate::clips::mux::nal_units;

fn nal_type(unit: &[u8]) -> u8 {
    unit.first().map_or(0, |b| b & 0x1f)
}

/// The decoder configuration of H.264 for a stream: the first SPS and PPS of a key frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoConfig {
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
}

impl VideoConfig {
    /// Reads the SPS and PPS out of an Annex B picture; `None` when it carries none.
    pub fn from_annex_b(data: &[u8]) -> Option<VideoConfig> {
        let units = nal_units(data);
        let sps = units.iter().find(|u| nal_type(u) == 7)?;
        let pps = units.iter().find(|u| nal_type(u) == 8)?;
        (sps.len() >= 4).then(|| VideoConfig {
            sps: sps.to_vec(),
            pps: pps.to_vec(),
        })
    }

    /// The tag that tells a player how to read the pictures that follow.
    pub fn tag(&self) -> Vec<u8> {
        let mut out = vec![0x17, 0, 0, 0, 0];
        out.extend_from_slice(&[1, self.sps[1], self.sps[2], self.sps[3], 0xFF, 0xE1]);
        out.extend_from_slice(&(self.sps.len() as u16).to_be_bytes());
        out.extend_from_slice(&self.sps);
        out.push(1);
        out.extend_from_slice(&(self.pps.len() as u16).to_be_bytes());
        out.extend_from_slice(&self.pps);
        out
    }
}

/// The tag of one picture: its slices (and SEI) length-prefixed. SPS, PPS and access unit
/// delimiters stay out, the configuration carries the first two. `None` when nothing is left.
pub fn video_tag(annex_b: &[u8], key: bool) -> Option<Vec<u8>> {
    let mut out = vec![if key { 0x17 } else { 0x27 }, 1, 0, 0, 0];
    let start = out.len();
    for unit in nal_units(annex_b) {
        if matches!(nal_type(unit), 7..=9) {
            continue;
        }
        out.extend_from_slice(&(unit.len() as u32).to_be_bytes());
        out.extend_from_slice(unit);
    }
    (out.len() > start).then_some(out)
}

/// AAC-LC, 48 kHz, stereo: the audio specific config.
pub const AAC_CONFIG: [u8; 2] = [0x11, 0x90];
/// One AAC frame of digital silence in that format (what a real encoder gives for zeros).
pub const AAC_SILENCE: [u8; 6] = [0x21, 0x10, 0x04, 0x60, 0x8C, 0x1C];
/// Samples in one AAC frame, and the rate.
pub const AAC_FRAME_SAMPLES: u64 = 1024;
pub const AAC_RATE: u64 = 48_000;

/// The audio configuration tag.
pub fn audio_config_tag() -> Vec<u8> {
    vec![0xAF, 0, AAC_CONFIG[0], AAC_CONFIG[1]]
}

/// A tag of one raw AAC frame.
pub fn audio_tag(frame: &[u8]) -> Vec<u8> {
    let mut out = vec![0xAF, 1];
    out.extend_from_slice(frame);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn annex_b() -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&[0, 0, 0, 1, 0x09, 0xF0]);
        data.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x42, 0xC0, 0x1F, 0xAA]);
        data.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xCE, 0x3C]);
        data.extend_from_slice(&[0, 0, 1, 0x65, 1, 2, 3]);
        data
    }

    #[test]
    fn the_configuration_comes_from_the_key_frame() {
        let config = VideoConfig::from_annex_b(&annex_b()).unwrap();
        assert_eq!(config.sps, [0x67, 0x42, 0xC0, 0x1F, 0xAA]);
        assert_eq!(config.pps, [0x68, 0xCE, 0x3C]);
        let tag = config.tag();
        assert_eq!(&tag[..5], [0x17, 0, 0, 0, 0]);
        // version, profile, compatibility, level
        assert_eq!(&tag[5..9], [1, 0x42, 0xC0, 0x1F]);
        assert_eq!(&tag[9..11], [0xFF, 0xE1]);
        assert_eq!(&tag[11..13], [0, 5]);
        assert!(VideoConfig::from_annex_b(&[0, 0, 1, 0x65, 1]).is_none());
    }

    #[test]
    fn a_picture_keeps_its_slices_length_prefixed() {
        let tag = video_tag(&annex_b(), true).unwrap();
        assert_eq!(tag, [0x17, 1, 0, 0, 0, 0, 0, 0, 4, 0x65, 1, 2, 3]);
        let delta = video_tag(&[0, 0, 1, 0x41, 9], false).unwrap();
        assert_eq!(delta, [0x27, 1, 0, 0, 0, 0, 0, 0, 2, 0x41, 9]);
        assert!(video_tag(&[0, 0, 1, 0x09, 0xF0], false).is_none());
    }
}
