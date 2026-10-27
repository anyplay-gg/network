//! The H.264 headers a VA-API driver wants as "packed headers": the sequence parameter set, the
//! picture parameter set and the slice header, written bit by bit (ITU-T H.264 7.3.2.1, 7.3.2.2,
//! 7.3.3). Pure code, tested on every system.
//!
//! What it writes is the one stream shape the worker's hardware encoders produce: frame coding
//! only, one slice per picture, one reference frame, no B pictures, CAVLC, picture order count
//! type 0 with a count that rises by two per picture, SDR BT.709 in limited range (VUI), and a
//! VUI that tells a decoder nothing is reordered (`max_num_reorder_frames` 0) so that it shows
//! each picture as soon as it has it.

/// `log2_max_frame_num_minus4`: `frame_num` has 8 bits and wraps at 256.
pub const LOG2_MAX_FRAME_NUM_MINUS4: u32 = 4;
/// `log2_max_pic_order_cnt_lsb_minus4`: the count's low part has 8 bits.
pub const LOG2_MAX_POC_LSB_MINUS4: u32 = 4;

/// Writes bits, most significant first.
#[derive(Default, Debug)]
pub struct BitWriter {
    bytes: Vec<u8>,
    /// Bits used in the last byte of `bytes` (0 when it is complete or `bytes` is empty).
    used: u32,
}

impl BitWriter {
    pub fn new() -> BitWriter {
        BitWriter::default()
    }

    pub fn bit(&mut self, bit: bool) {
        if self.used == 0 {
            self.bytes.push(0);
        }
        if bit {
            let last = self.bytes.len() - 1;
            self.bytes[last] |= 0x80 >> self.used;
        }
        self.used = (self.used + 1) % 8;
    }

    /// The low `count` bits of `value` (at most 32), u(n).
    pub fn bits(&mut self, value: u32, count: u32) {
        assert!(count <= 32);
        for shift in (0..count).rev() {
            self.bit((value >> shift) & 1 == 1);
        }
    }

    /// ue(v): unsigned Exp-Golomb.
    pub fn ue(&mut self, value: u32) {
        let coded = u64::from(value) + 1;
        let length = 64 - coded.leading_zeros(); // bits of `coded`
        for _ in 1..length {
            self.bit(false);
        }
        for shift in (0..length).rev() {
            self.bit((coded >> shift) & 1 == 1);
        }
    }

    /// se(v): signed Exp-Golomb (positive values take the odd codes).
    pub fn se(&mut self, value: i32) {
        let mapped = if value > 0 {
            (value as u32) * 2 - 1
        } else {
            value.unsigned_abs() * 2
        };
        self.ue(mapped);
    }

    /// How many bits were written.
    pub fn bit_len(&self) -> u32 {
        if self.used == 0 {
            self.bytes.len() as u32 * 8
        } else {
            (self.bytes.len() as u32 - 1) * 8 + self.used
        }
    }

    /// rbsp_trailing_bits(): a one, then zeros up to the byte boundary.
    pub fn trailing(&mut self) {
        self.bit(true);
        while self.used != 0 {
            self.bit(false);
        }
    }

    /// The bytes; a last partial byte is padded with zero bits.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// Inserts the emulation prevention byte `03` into `rbsp` wherever two zero bytes would be
/// followed by a byte of 0 to 3.
pub fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 4);
    let mut zeros = 0;
    for &byte in rbsp {
        if zeros >= 2 && byte <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// A NAL unit in Annex B form: the four-byte start code, the header byte, and the payload with
/// emulation prevention.
pub fn nal(ref_idc: u8, nal_type: u8, rbsp: &[u8]) -> Vec<u8> {
    let mut out = vec![0, 0, 0, 1, (ref_idc << 5) | nal_type];
    out.extend(escape(rbsp));
    out
}

/// The H.264 profiles the hardware encoders are asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// Constrained Baseline.
    Baseline,
    Main,
    High,
}

impl Profile {
    pub fn idc(self) -> u32 {
        match self {
            Profile::Baseline => 66,
            Profile::Main => 77,
            Profile::High => 100,
        }
    }

    /// The constraint flags byte: constraint_set1 for Constrained Baseline.
    fn constraints(self) -> u32 {
        match self {
            Profile::Baseline => 0x40,
            _ => 0,
        }
    }
}

/// The smallest level (`level_idc`) that holds a picture of `width_mbs` by `height_mbs`
/// macroblocks at `fps` (Table A-1: frame size and macroblock rate).
pub fn level_for(width_mbs: u32, height_mbs: u32, fps: u32) -> u8 {
    const LEVELS: [(u8, u32, u32); 8] = [
        (30, 40_500, 1_620),
        (31, 108_000, 3_600),
        (32, 216_000, 5_120),
        (41, 245_760, 8_192),
        (42, 522_240, 8_704),
        (50, 589_824, 22_080),
        (51, 983_040, 36_864),
        (52, 2_073_600, 36_864),
    ];
    let size = width_mbs * height_mbs;
    let rate = size.saturating_mul(fps);
    LEVELS
        .iter()
        .find(|&&(_, max_rate, max_size)| size <= max_size && rate <= max_rate)
        .map_or(52, |l| l.0)
}

/// What the sequence parameter set says.
#[derive(Clone, Copy, Debug)]
pub struct Sps {
    pub profile: Profile,
    pub level_idc: u8,
    pub width_mbs: u32,
    pub height_mbs: u32,
    /// Luma pixels cut off the right and the bottom (the picture is a whole number of
    /// macroblocks; even numbers).
    pub crop_right: u32,
    pub crop_bottom: u32,
    pub fps: u32,
}

/// The sequence parameter set's payload (without NAL header and emulation prevention).
pub fn sps_rbsp(sps: &Sps) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.bits(sps.profile.idc(), 8);
    w.bits(sps.profile.constraints(), 8);
    w.bits(u32::from(sps.level_idc), 8);
    w.ue(0); // seq_parameter_set_id
    if sps.profile == Profile::High {
        w.ue(1); // chroma_format_idc: 4:2:0
        w.ue(0); // bit_depth_luma_minus8
        w.ue(0); // bit_depth_chroma_minus8
        w.bit(false); // qpprime_y_zero_transform_bypass_flag
        w.bit(false); // seq_scaling_matrix_present_flag
    }
    w.ue(LOG2_MAX_FRAME_NUM_MINUS4);
    w.ue(0); // pic_order_cnt_type
    w.ue(LOG2_MAX_POC_LSB_MINUS4);
    w.ue(1); // max_num_ref_frames
    w.bit(false); // gaps_in_frame_num_value_allowed_flag
    w.ue(sps.width_mbs - 1);
    w.ue(sps.height_mbs - 1); // pic_height_in_map_units_minus1 (frames only)
    w.bit(true); // frame_mbs_only_flag
    w.bit(true); // direct_8x8_inference_flag
    let cropped = sps.crop_right > 0 || sps.crop_bottom > 0;
    w.bit(cropped);
    if cropped {
        // In units of two luma pixels (4:2:0, frames only).
        w.ue(0);
        w.ue(sps.crop_right / 2);
        w.ue(0);
        w.ue(sps.crop_bottom / 2);
    }
    w.bit(true); // vui_parameters_present_flag
    vui(&mut w, sps.fps);
    w.trailing();
    w.into_bytes()
}

/// The VUI: BT.709 limited range, a fixed frame rate, and a decoder that never reorders.
fn vui(w: &mut BitWriter, fps: u32) {
    w.bit(false); // aspect_ratio_info_present_flag
    w.bit(false); // overscan_info_present_flag
    w.bit(true); // video_signal_type_present_flag
    w.bits(5, 3); // video_format: unspecified
    w.bit(false); // video_full_range_flag: limited
    w.bit(true); // colour_description_present_flag
    w.bits(1, 8); // colour_primaries: BT.709
    w.bits(1, 8); // transfer_characteristics: BT.709
    w.bits(1, 8); // matrix_coefficients: BT.709
    w.bit(false); // chroma_loc_info_present_flag
    w.bit(true); // timing_info_present_flag
    w.bits(1, 32); // num_units_in_tick
    w.bits(fps.max(1) * 2, 32); // time_scale: two ticks per frame
    w.bit(true); // fixed_frame_rate_flag
    w.bit(false); // nal_hrd_parameters_present_flag
    w.bit(false); // vcl_hrd_parameters_present_flag
    w.bit(false); // pic_struct_present_flag
    w.bit(true); // bitstream_restriction_flag
    w.bit(true); // motion_vectors_over_pic_boundaries_flag
    w.ue(0); // max_bytes_per_pic_denom
    w.ue(0); // max_bits_per_mb_denom
    w.ue(15); // log2_max_mv_length_horizontal
    w.ue(15); // log2_max_mv_length_vertical
    w.ue(0); // max_num_reorder_frames
    w.ue(1); // max_dec_frame_buffering
}

/// The sequence parameter set as a NAL unit.
pub fn sps_nal(sps: &Sps) -> Vec<u8> {
    nal(3, 7, &sps_rbsp(sps))
}

/// What the picture parameter set says.
#[derive(Clone, Copy, Debug)]
pub struct Pps {
    pub profile: Profile,
    pub init_qp: u32,
    /// `transform_8x8_mode_flag`, High profile only.
    pub transform_8x8: bool,
}

/// The picture parameter set's payload.
pub fn pps_rbsp(pps: &Pps) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.ue(0); // pic_parameter_set_id
    w.ue(0); // seq_parameter_set_id
    w.bit(false); // entropy_coding_mode_flag: CAVLC
    w.bit(false); // bottom_field_pic_order_in_frame_present_flag
    w.ue(0); // num_slice_groups_minus1
    w.ue(0); // num_ref_idx_l0_default_active_minus1
    w.ue(0); // num_ref_idx_l1_default_active_minus1
    w.bit(false); // weighted_pred_flag
    w.bits(0, 2); // weighted_bipred_idc
    w.se(pps.init_qp as i32 - 26); // pic_init_qp_minus26
    w.se(0); // pic_init_qs_minus26
    w.se(0); // chroma_qp_index_offset
    w.bit(true); // deblocking_filter_control_present_flag
    w.bit(false); // constrained_intra_pred_flag
    w.bit(false); // redundant_pic_cnt_present_flag
    if pps.profile == Profile::High {
        w.bit(pps.transform_8x8);
        w.bit(false); // pic_scaling_matrix_present_flag
        w.se(0); // second_chroma_qp_index_offset
    }
    w.trailing();
    w.into_bytes()
}

/// The picture parameter set as a NAL unit.
pub fn pps_nal(pps: &Pps) -> Vec<u8> {
    nal(3, 8, &pps_rbsp(pps))
}

/// What a slice header says (one slice per picture).
#[derive(Clone, Copy, Debug)]
pub struct Slice {
    /// An IDR picture (an I slice that starts a new sequence); else a P slice.
    pub idr: bool,
    pub frame_num: u32,
    pub idr_pic_id: u32,
    pub poc_lsb: u32,
    pub qp_delta: i32,
}

/// A packed header: the bytes (Annex B, with emulation prevention) and the number of bits in
/// them that count. The last byte of a slice header is only partly used: the driver continues
/// the bits there with the slice data.
#[derive(Debug, PartialEq, Eq)]
pub struct Packed {
    pub bytes: Vec<u8>,
    pub bit_length: u32,
}

/// A whole NAL unit as a packed header.
pub fn packed(nal: Vec<u8>) -> Packed {
    Packed {
        bit_length: nal.len() as u32 * 8,
        bytes: nal,
    }
}

/// The slice header as a packed header: start code, NAL header and the header's bits, without
/// trailing bits.
pub fn slice_header(slice: &Slice) -> Packed {
    let mut w = BitWriter::new();
    w.ue(0); // first_mb_in_slice
    w.ue(if slice.idr { 2 } else { 0 }); // slice_type: I or P
    w.ue(0); // pic_parameter_set_id
    w.bits(slice.frame_num, LOG2_MAX_FRAME_NUM_MINUS4 + 4);
    if slice.idr {
        w.ue(slice.idr_pic_id);
    }
    w.bits(slice.poc_lsb, LOG2_MAX_POC_LSB_MINUS4 + 4);
    if !slice.idr {
        w.bit(false); // num_ref_idx_active_override_flag
        w.bit(false); // ref_pic_list_modification_flag_l0
    }
    // dec_ref_pic_marking(): every picture is a reference.
    if slice.idr {
        w.bit(false); // no_output_of_prior_pics_flag
        w.bit(false); // long_term_reference_flag
    } else {
        w.bit(false); // adaptive_ref_pic_marking_mode_flag
    }
    w.se(slice.qp_delta);
    w.ue(0); // disable_deblocking_filter_idc: filter on
    w.se(0); // slice_alpha_c0_offset_div2
    w.se(0); // slice_beta_offset_div2
    let bits = w.bit_len();
    let rbsp = w.into_bytes();
    let (ref_idc, nal_type) = if slice.idr { (3, 5) } else { (2, 1) };
    // Escape the complete bytes; the last, partial one gets slice data bits and stays as is.
    let whole = if bits.is_multiple_of(8) {
        rbsp.len()
    } else {
        rbsp.len() - 1
    };
    let mut bytes = vec![0, 0, 0, 1, (ref_idc << 5) | nal_type];
    let escaped = escape(&rbsp[..whole]);
    let bit_length = 40 + escaped.len() as u32 * 8 + bits % 8;
    bytes.extend(escaped);
    bytes.extend_from_slice(&rbsp[whole..]);
    Packed { bytes, bit_length }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bytes from a string of 0 and 1 (spaces ignored), padded with zeros.
    fn from_bits(text: &str) -> Vec<u8> {
        let mut w = BitWriter::new();
        for c in text.chars().filter(|c| !c.is_whitespace()) {
            w.bit(c == '1');
        }
        w.into_bytes()
    }

    #[test]
    fn exp_golomb_codes_are_the_standard_s() {
        let mut w = BitWriter::new();
        for v in [0, 1, 2, 3, 4, 7, 8] {
            w.ue(v);
        }
        // 1 010 011 00100 00101 0001000 0001001
        assert_eq!(
            w.into_bytes(),
            from_bits("1 010 011 00100 00101 0001000 0001001")
        );

        let mut w = BitWriter::new();
        for v in [0, 1, -1, 2, -2, 3] {
            w.se(v);
        }
        // se maps 0,1,-1,2,-2,3 to ue 0,1,2,3,4,5
        assert_eq!(w.into_bytes(), from_bits("1 010 011 00100 00101 00110"));
    }

    #[test]
    fn bit_length_and_trailing_bits() {
        let mut w = BitWriter::new();
        w.bits(0b101, 3);
        assert_eq!(w.bit_len(), 3);
        w.trailing();
        assert_eq!(w.bit_len(), 8);
        assert_eq!(w.into_bytes(), [0b1011_0000]);
        let mut w = BitWriter::new();
        w.bits(0xff, 8);
        assert_eq!(w.bit_len(), 8);
        w.trailing();
        assert_eq!(w.into_bytes(), [0xff, 0x80]);
    }

    #[test]
    fn emulation_prevention_is_inserted_after_two_zeros() {
        assert_eq!(escape(&[0, 0, 1]), [0, 0, 3, 1]);
        assert_eq!(escape(&[0, 0, 0, 0]), [0, 0, 3, 0, 0]);
        assert_eq!(escape(&[0, 0, 3]), [0, 0, 3, 3]);
        assert_eq!(escape(&[0, 0, 4, 0, 1]), [0, 0, 4, 0, 1]);
        assert_eq!(escape(&[0, 1, 0, 0, 2]), [0, 1, 0, 0, 3, 2]);
    }

    #[test]
    fn a_720p30_constrained_baseline_sps_matches_the_bits_derived_by_hand() {
        let sps = Sps {
            profile: Profile::Baseline,
            level_idc: 31,
            width_mbs: 80,
            height_mbs: 45,
            crop_right: 0,
            crop_bottom: 0,
            fps: 30,
        };
        let expected = from_bits(
            "01000010 01000000 00011111 \
             1 \
             00101 1 00101 010 0 \
             0000001010000 00000101101 \
             1 1 0 \
             1 \
             0 0 1 101 0 1 00000001 00000001 00000001 0 \
             1 00000000000000000000000000000001 00000000000000000000000000111100 1 \
             0 0 0 \
             1 1 1 1 000010000 000010000 1 010 \
             1",
        );
        assert_eq!(sps_rbsp(&sps), expected);
        let bytes = sps_nal(&sps);
        assert_eq!(&bytes[..8], [0, 0, 0, 1, 0x67, 0x42, 0x40, 0x1f]);
    }

    #[test]
    fn a_high_profile_sps_names_its_chroma_format_and_crops_1080p() {
        let sps = Sps {
            profile: Profile::High,
            level_idc: 42,
            width_mbs: 120,
            height_mbs: 68,
            crop_right: 0,
            crop_bottom: 8,
            fps: 60,
        };
        let rbsp = sps_rbsp(&sps);
        assert_eq!(&rbsp[..3], [100, 0, 42]);
        // sps id 1, chroma_format_idc ue(1)=010, two bit depths 1 1, two flags 0 0,
        // then log2_max_frame_num_minus4 ue(4)=00101: 1 010 1 1 0 0 00101...
        assert_eq!(rbsp[3], 0b1010_1100);
        // The crop is in the stream: frame_cropping_flag 1 and bottom offset 4.
        let plain = sps_rbsp(&Sps {
            crop_bottom: 0,
            ..sps
        });
        assert_ne!(rbsp, plain);
    }

    #[test]
    fn the_pps_for_each_profile_matches_the_bits_derived_by_hand() {
        let main = Pps {
            profile: Profile::Main,
            init_qp: 26,
            transform_8x8: false,
        };
        // id 0, sps 0, cavlc 0, bottom 0, groups 0, l0 0, l1 0 (as ue(0) = 1 each where ue),
        // weighted 0, bipred 00, qp 26 -> se(0) 1, qs 1, chroma 1, deblock 1, cip 0, redundant 0.
        assert_eq!(
            pps_rbsp(&main),
            from_bits("1 1 0 0 1 1 1 0 00 1 1 1 1 0 0 1")
        );
        assert_eq!(&pps_nal(&main)[..5], [0, 0, 0, 1, 0x68]);
        let high = Pps {
            profile: Profile::High,
            init_qp: 30,
            transform_8x8: true,
        };
        // qp 30 -> se(4) -> ue(7) = 0001000; qs, chroma, deblock 1 1 1, cip 0, redundant 0,
        // transform8x8 1, scaling 0, se(0) 1, trailing 1.
        assert_eq!(
            pps_rbsp(&high),
            from_bits("1 1 0 0 1 1 1 0 00 0001000 1 1 1 0 0 1 0 1 1")
        );
    }

    #[test]
    fn slice_headers_match_the_bits_derived_by_hand() {
        // IDR: first_mb 1, slice_type I ue(2)=011, pps 1, frame_num 00000000, idr_pic_id ue(1)
        // 010, poc lsb 00000000, marking 0 0, qp_delta se(0) 1, deblock ue(0) 1, se(0) 1, 1.
        let idr = slice_header(&Slice {
            idr: true,
            frame_num: 0,
            idr_pic_id: 1,
            poc_lsb: 0,
            qp_delta: 0,
        });
        let bits = "1 011 1 00000000 010 00000000 0 0 1 1 1 1";
        assert_eq!(idr.bit_length, 40 + bits.replace(' ', "").len() as u32);
        assert_eq!(&idr.bytes[..5], [0, 0, 0, 1, 0x65]);
        assert_eq!(&idr.bytes[5..], &from_bits(bits)[..]);

        // P: first_mb 1, type P ue(0) 1, pps 1, frame_num 5 = 00000101, poc 10 = 00001010,
        // override 0, modification 0, marking 0, qp_delta se(-2) = ue(4) 00101, 1, 1, 1.
        let p = slice_header(&Slice {
            idr: false,
            frame_num: 5,
            idr_pic_id: 0,
            poc_lsb: 10,
            qp_delta: -2,
        });
        let bits = "1 1 1 00000101 00001010 0 0 0 00101 1 1 1";
        assert_eq!(&p.bytes[..5], [0, 0, 0, 1, 0x41]);
        assert_eq!(&p.bytes[5..], &from_bits(bits)[..]);
        assert_eq!(p.bit_length, 40 + bits.replace(' ', "").len() as u32);
    }

    #[test]
    fn a_slice_header_counts_its_bits_when_it_has_zero_bytes() {
        // A frame_num and POC of zero make zero bytes; the header must not contain 00 00 0x
        // after the start code, and its bit count stays within its last byte.
        let h = slice_header(&Slice {
            idr: false,
            frame_num: 0,
            idr_pic_id: 0,
            poc_lsb: 0,
            qp_delta: 0,
        });
        let body = &h.bytes[4..];
        assert!(
            body.windows(3)
                .all(|w| !(w[0] == 0 && w[1] == 0 && w[2] <= 2)),
            "{body:02x?}"
        );
        assert!(h.bit_length <= h.bytes.len() as u32 * 8);
        assert!(h.bit_length + 8 > h.bytes.len() as u32 * 8);
    }

    #[test]
    fn levels_follow_the_size_and_rate() {
        assert_eq!(level_for(80, 45, 30), 31); // 720p30
        assert_eq!(level_for(120, 68, 30), 41); // 1080p30: 8160 MBs, 244800/s
        assert_eq!(level_for(120, 68, 60), 42); // 489600/s
        assert_eq!(level_for(40, 23, 30), 30);
        assert_eq!(level_for(240, 135, 30), 51); // 4K
    }
}
