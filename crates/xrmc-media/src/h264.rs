//! H.264 / AVC: codec strings, SPS parsing and parameter-set extraction.

use crate::MediaError;
use crate::bits::BitReader;
use crate::nal::{nal_offsets, split_nals, unescape_rbsp};

/// Codec string used when a config packet carries no SPS (High, level 5.1).
pub const DEFAULT_CODEC: &str = "avc1.640033";

/// NAL unit types the muxer cares about.
pub mod nal_type {
    /// Coded slice of an IDR picture.
    pub const IDR: u8 = 5;
    /// Sequence parameter set.
    pub const SPS: u8 = 7;
    /// Picture parameter set.
    pub const PPS: u8 = 8;
    /// Access unit delimiter.
    pub const AUD: u8 = 9;
}

/// `avc1.PPCCLL` codec string from the first SPS in an Annex B buffer
/// (profile_idc, constraint flags, level_idc), or [`DEFAULT_CODEC`].
pub fn codec_string(data: &[u8]) -> String {
    for off in nal_offsets(data) {
        if data[off] & 0x1f == nal_type::SPS && off + 3 < data.len() {
            return format!(
                "avc1.{:02x}{:02x}{:02x}",
                data[off + 1],
                data[off + 2],
                data[off + 3]
            );
        }
    }
    DEFAULT_CODEC.to_owned()
}

/// What an H.264 SPS says about the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpsInfo {
    /// `profile_idc` (66 Baseline, 77 Main, 100 High, 110 High 10, ...).
    pub profile: u8,
    /// The constraint_set flags byte.
    pub compat: u8,
    /// `level_idc` (e.g. 51 for level 5.1).
    pub level: u8,
    /// `chroma_format_idc` (1 = 4:2:0).
    pub chroma_format_idc: u32,
    /// Luma bit depth (8 unless a High 10/4:2:2/4:4:4 profile says otherwise).
    pub bit_depth_luma: u8,
    /// Chroma bit depth.
    pub bit_depth_chroma: u8,
    /// Display width in pixels (frame cropping applied).
    pub width: u32,
    /// Display height in pixels (frame cropping applied).
    pub height: u32,
}

const HIGH_PROFILES: [u32; 13] = [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

/// Parse an SPS NAL unit (including its 1-byte NAL header, without start code).
pub fn parse_sps(nal: &[u8]) -> Result<SpsInfo, MediaError> {
    let rbsp = unescape_rbsp(nal);
    let mut r = BitReader::new(&rbsp);
    r.bits(8)?; // NAL header
    let profile = r.bits(8)?;
    let compat = r.bits(8)?;
    let level = r.bits(8)?;
    r.ue()?; // seq_parameter_set_id
    let mut chroma = 1;
    let mut depth_luma = 8;
    let mut depth_chroma = 8;
    if HIGH_PROFILES.contains(&profile) {
        chroma = r.ue()?;
        if chroma > 3 {
            return Err(MediaError::Invalid("bad chroma_format_idc"));
        }
        if chroma == 3 {
            r.bit()?; // separate_colour_plane_flag
        }
        depth_luma = r.ue()? + 8;
        depth_chroma = r.ue()? + 8;
        if depth_luma > 16 || depth_chroma > 16 {
            return Err(MediaError::Invalid("bad bit depth"));
        }
        r.bit()?; // qpprime_y_zero_transform_bypass_flag
        if r.flag()? {
            // seq_scaling_matrix_present_flag: skip the scaling lists
            let lists = if chroma != 3 { 8 } else { 12 };
            for i in 0..lists {
                if !r.flag()? {
                    continue;
                }
                let size = if i < 6 { 16 } else { 64 };
                let mut last: i32 = 8;
                let mut next: i32 = 8;
                for _ in 0..size {
                    if next != 0 {
                        next = (last + r.se()? + 256).rem_euclid(256);
                    }
                    last = if next == 0 { last } else { next };
                }
            }
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    let poc_type = r.ue()?;
    if poc_type == 0 {
        r.ue()?; // log2_max_pic_order_cnt_lsb_minus4
    } else if poc_type == 1 {
        r.bit()?; // delta_pic_order_always_zero_flag
        r.se()?; // offset_for_non_ref_pic
        r.se()?; // offset_for_top_to_bottom_field
        let n = r.ue()?;
        if n > 255 {
            return Err(MediaError::Invalid("too many ref frames in POC cycle"));
        }
        for _ in 0..n {
            r.se()?;
        }
    }
    r.ue()?; // max_num_ref_frames
    r.bit()?; // gaps_in_frame_num_value_allowed_flag
    let w_mbs = u64::from(r.ue()?) + 1;
    let h_map_units = u64::from(r.ue()?) + 1;
    let frame_mbs_only = u64::from(r.bit()?);
    if frame_mbs_only == 0 {
        r.bit()?; // mb_adaptive_frame_field_flag
    }
    r.bit()?; // direct_8x8_inference_flag
    let (mut cl, mut cr, mut ct, mut cb) = (0u64, 0u64, 0u64, 0u64);
    if r.flag()? {
        cl = r.ue()?.into();
        cr = r.ue()?.into();
        ct = r.ue()?.into();
        cb = r.ue()?.into();
    }
    let sub_w = if chroma == 1 || chroma == 2 { 2 } else { 1 };
    let sub_h = if chroma == 1 { 2 } else { 1 };
    let crop_x = if chroma == 0 { 1 } else { sub_w };
    let crop_y = (if chroma == 0 { 1 } else { sub_h }) * (2 - frame_mbs_only);
    let width = (w_mbs * 16).checked_sub((cl + cr) * crop_x);
    let height = ((2 - frame_mbs_only) * h_map_units * 16).checked_sub((ct + cb) * crop_y);
    let (Some(width), Some(height)) = (width, height) else {
        return Err(MediaError::Invalid("cropping larger than the picture"));
    };
    let to_u32 = |v: u64| u32::try_from(v).ok().filter(|v| *v > 0);
    let (Some(width), Some(height)) = (to_u32(width), to_u32(height)) else {
        return Err(MediaError::Invalid("bad picture size"));
    };
    Ok(SpsInfo {
        profile: profile as u8,
        compat: compat as u8,
        level: level as u8,
        chroma_format_idc: chroma,
        bit_depth_luma: depth_luma as u8,
        bit_depth_chroma: depth_chroma as u8,
        width,
        height,
    })
}

/// Parameter sets and stream info from an Annex B config packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvcConfig {
    /// SPS NAL units (with NAL header, without start codes). Never empty.
    pub sps: Vec<Vec<u8>>,
    /// PPS NAL units. Never empty.
    pub pps: Vec<Vec<u8>>,
    /// Parsed first SPS.
    pub info: SpsInfo,
    /// `avc1.PPCCLL` codec string.
    pub codec: String,
}

impl AvcConfig {
    /// Display width in pixels.
    pub fn width(&self) -> u32 {
        self.info.width
    }

    /// Display height in pixels.
    pub fn height(&self) -> u32 {
        self.info.height
    }

    /// The AVCDecoderConfigurationRecord (the payload of an `avcC` box), as
    /// used for MP4 and by platform decoders that take out-of-band parameters.
    pub fn decoder_configuration_record(&self) -> Vec<u8> {
        let sps = &self.sps[..self.sps.len().min(31)];
        let pps = &self.pps[..self.pps.len().min(255)];
        let first = &self.sps[0];
        let mut out = vec![
            1,
            first[1],
            first[2],
            first[3],
            0xff,
            0xe0 | sps.len() as u8,
        ];
        for s in sps {
            out.extend_from_slice(&(s.len() as u16).to_be_bytes());
            out.extend_from_slice(s);
        }
        out.push(pps.len() as u8);
        for p in pps {
            out.extend_from_slice(&(p.len() as u16).to_be_bytes());
            out.extend_from_slice(p);
        }
        out
    }
}

/// Extract SPS/PPS from an Annex B buffer (a config packet, or a key frame
/// carrying its parameter sets in-band) and parse the first SPS.
///
/// Returns [`MediaError::NoParameterSets`] when there is no SPS or no PPS.
pub fn parse_config(data: &[u8]) -> Result<AvcConfig, MediaError> {
    let nals = split_nals(data);
    let of_type = |t: u8| -> Vec<Vec<u8>> {
        nals.iter()
            .filter(|n| n.first().is_some_and(|h| h & 0x1f == t))
            .map(|n| n.to_vec())
            .collect()
    };
    let sps = of_type(nal_type::SPS);
    let pps = of_type(nal_type::PPS);
    if sps.is_empty() || pps.is_empty() {
        return Err(MediaError::NoParameterSets);
    }
    if sps
        .iter()
        .chain(&pps)
        .any(|n| n.len() > usize::from(u16::MAX))
    {
        return Err(MediaError::Invalid("parameter set larger than 64 KiB"));
    }
    let info = parse_sps(&sps[0])?;
    let codec = format!("avc1.{:02x}{:02x}{:02x}", sps[0][1], sps[0][2], sps[0][3]);
    Ok(AvcConfig {
        sps,
        pps,
        info,
        codec,
    })
}

#[cfg(test)]
pub(crate) mod test_util {
    /// Minimal Exp-Golomb bit writer to build test SPS NAL units
    /// (port of the helper in `test/fmp4.test.js`).
    #[derive(Default)]
    pub struct BitWriter {
        bits: Vec<u8>,
    }

    impl BitWriter {
        pub fn u(mut self, n: u32, v: u64) -> Self {
            for i in (0..n).rev() {
                self.bits.push(((v >> i) & 1) as u8);
            }
            self
        }

        pub fn ue(self, v: u64) -> Self {
            let x = v + 1;
            let len = 63 - x.leading_zeros();
            let mut w = self;
            for _ in 0..len {
                w.bits.push(0);
            }
            w.u(len + 1, x)
        }

        pub fn bytes(mut self) -> Vec<u8> {
            self.bits.push(1); // rbsp stop bit
            while !self.bits.len().is_multiple_of(8) {
                self.bits.push(0);
            }
            self.bits
                .chunks(8)
                .map(|c| c.iter().fold(0u8, |a, b| (a << 1) | b))
                .collect()
        }
    }

    /// High-profile (or Baseline with `profile = 66`) SPS for width x height
    /// (16-aligned macroblocks, cropped).
    pub fn make_sps(width: u32, height: u32, profile: u8) -> Vec<u8> {
        let mb_w = width.div_ceil(16);
        let mb_h = height.div_ceil(16);
        let mut w = BitWriter::default()
            .u(8, 0x67)
            .u(8, profile.into())
            .u(8, 0)
            .u(8, 51)
            .ue(0);
        if profile == 100 {
            w = w.ue(1).ue(0).ue(0).u(1, 0).u(1, 0);
        }
        w = w
            .ue(4)
            .ue(2)
            .ue(1)
            .u(1, 0)
            .ue((mb_w - 1).into())
            .ue((mb_h - 1).into())
            .u(1, 1)
            .u(1, 1);
        let crop_r = (mb_w * 16 - width) / 2;
        let crop_b = (mb_h * 16 - height) / 2;
        if crop_r != 0 || crop_b != 0 {
            w = w.u(1, 1).ue(0).ue(crop_r.into()).ue(0).ue(crop_b.into());
        } else {
            w = w.u(1, 0);
        }
        w.u(1, 0).bytes()
    }

    /// Annex B buffer with 4-byte start codes.
    pub fn annex_b(nals: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for n in nals {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(n);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::*;
    use super::*;

    // Port of 'SPS parser reads dimensions, including cropping' (test/fmp4.test.js).
    #[test]
    fn sps_parser_reads_dimensions_including_cropping() {
        for (w, h) in [
            (1920, 1080),
            (1832, 1920),
            (3664, 1920),
            (1280, 720),
            (2064, 2208),
        ] {
            let info = parse_sps(&make_sps(w, h, 100)).unwrap();
            assert_eq!(
                (info.width, info.height, info.profile, info.level),
                (w, h, 100, 51)
            );
            assert_eq!(info.bit_depth_luma, 8);
        }
        let base = parse_sps(&make_sps(640, 480, 66)).unwrap();
        assert_eq!((base.width, base.height, base.profile), (640, 480, 66));
    }

    #[test]
    fn high10_bit_depth() {
        let sps = BitWriter::default()
            .u(8, 0x67)
            .u(8, 110)
            .u(8, 0)
            .u(8, 51)
            .ue(0)
            .ue(1) // 4:2:0
            .ue(2) // luma 10-bit
            .ue(2) // chroma 10-bit
            .u(1, 0)
            .u(1, 0)
            .ue(4)
            .ue(2)
            .ue(1)
            .u(1, 0)
            .ue(79)
            .ue(44)
            .u(1, 1)
            .u(1, 1)
            .u(1, 0)
            .u(1, 0)
            .bytes();
        let info = parse_sps(&sps).unwrap();
        assert_eq!(
            (
                info.width,
                info.height,
                info.bit_depth_luma,
                info.bit_depth_chroma
            ),
            (1280, 720, 10, 10)
        );
    }

    #[test]
    fn scaling_lists_are_skipped() {
        // High profile with seq_scaling_matrix_present_flag and one explicit 4x4 list
        // whose deltas end the list early (nextScale == 0).
        let sps = BitWriter::default()
            .u(8, 0x67)
            .u(8, 100)
            .u(8, 0)
            .u(8, 40)
            .ue(0)
            .ue(1)
            .ue(0)
            .ue(0)
            .u(1, 0)
            .u(1, 1) // seq_scaling_matrix_present_flag
            .u(1, 1) // list 0 present
            .ue(16) // se(-8): 8 + -8 = 0 -> rest of the list uses last scale
            .u(7, 0) // lists 1..7 absent
            .ue(4)
            .ue(2)
            .ue(1)
            .u(1, 0)
            .ue(119)
            .ue(67)
            .u(1, 1)
            .u(1, 1)
            .u(1, 0)
            .u(1, 0)
            .bytes();
        let info = parse_sps(&sps).unwrap();
        assert_eq!((info.width, info.height), (1920, 1088));
    }

    #[test]
    fn codec_string_from_sps() {
        assert_eq!(
            codec_string(&[0, 0, 0, 1, 0x67, 0x64, 0x00, 0x33, 0xac]),
            "avc1.640033"
        );
        assert_eq!(codec_string(&[0, 0, 0, 1, 0x68, 0xee]), DEFAULT_CODEC);
        assert_eq!(codec_string(&[]), DEFAULT_CODEC);
    }

    // Port of 'config packet -> codec string and avcC record' (test/fmp4.test.js).
    #[test]
    fn config_packet_to_codec_string_and_avcc_record() {
        let sps = make_sps(1920, 1080, 100);
        let pps = [0x68, 0xee, 0x3c, 0x80];
        let cfg = parse_config(&annex_b(&[&sps, &pps])).unwrap();
        assert_eq!(cfg.codec, "avc1.640033");
        assert_eq!((cfg.width(), cfg.height()), (1920, 1080));
        let rec = cfg.decoder_configuration_record();
        assert_eq!(&rec[..6], &[1, 0x64, 0x00, 0x33, 0xff, 0xe1]);
        assert_eq!(u16::from_be_bytes([rec[6], rec[7]]) as usize, sps.len());
        assert_eq!(rec[8 + sps.len()], 1);
        assert!(matches!(
            parse_config(&annex_b(&[&pps])),
            Err(MediaError::NoParameterSets)
        ));
    }

    #[test]
    fn garbage_does_not_panic() {
        let mut seed = 0x1234_5678_u32;
        for len in 0..200 {
            let data: Vec<u8> = (0..len)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    seed as u8
                })
                .collect();
            let _ = parse_sps(&data);
            let mut with_header = vec![0x67];
            with_header.extend_from_slice(&data);
            let _ = parse_sps(&with_header);
            let _ = parse_config(&annex_b(&[&with_header, &[0x68, 1]]));
            let _ = codec_string(&data);
        }
    }
}
