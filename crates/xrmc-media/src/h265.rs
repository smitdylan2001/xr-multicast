//! H.265 / HEVC: RFC 6381 codec strings and SPS parsing.

use crate::MediaError;
use crate::bits::BitReader;
use crate::nal::{nal_offsets, unescape_rbsp_prefix};

/// Codec string used when a config packet carries no SPS (Main, level 5.1).
pub const DEFAULT_CODEC: &str = "hvc1.1.6.L153.B0";
/// Codec string to probe 10-bit decoding (Main 10, level 5.1).
pub const MAIN10_CODEC: &str = "hvc1.2.4.L153.B0";

/// NAL unit type of a sequence parameter set.
pub const NAL_SPS: u8 = 33;

/// The SPS fields up to and including the bit depths are well within this
/// many RBSP bytes, even with the maximum number of sub-layers.
const SPS_PREFIX_BYTES: usize = 256;

/// What an HEVC SPS says about the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HevcSps {
    /// `general_profile_space` (0..=3).
    pub profile_space: u8,
    /// `general_tier_flag` (true = High tier).
    pub high_tier: bool,
    /// `general_profile_idc` (1 Main, 2 Main 10, 3 Main Still Picture, 4 RExt, ...).
    pub profile_idc: u8,
    /// `general_profile_compatibility_flags` (flag j at bit `31 - j`).
    pub compat_flags: u32,
    /// The 48 bits of general constraint flags, as 6 bytes.
    pub constraint_flags: [u8; 6],
    /// `general_level_idc` (30 x level, e.g. 153 = level 5.1).
    pub level_idc: u8,
    /// `chroma_format_idc` (1 = 4:2:0).
    pub chroma_format_idc: u32,
    /// Luma bit depth.
    pub bit_depth_luma: u8,
    /// Chroma bit depth.
    pub bit_depth_chroma: u8,
    /// Display width in pixels (conformance window applied).
    pub width: u32,
    /// Display height in pixels (conformance window applied).
    pub height: u32,
}

/// Parse an SPS RBSP that starts just after the 2-byte NAL header.
fn parse_sps_rbsp(b: &[u8]) -> Result<HevcSps, MediaError> {
    if b.len() < 13 {
        return Err(MediaError::Truncated);
    }
    let mut r = BitReader::new(b);
    r.skip(4); // sps_video_parameter_set_id
    let max_sub_layers_minus1 = r.bits(3)? as usize;
    r.skip(1); // sps_temporal_id_nesting_flag
    r.skip(96); // general profile_tier_level + general_level_idc
    let mut sub_profile = [false; 8];
    let mut sub_level = [false; 8];
    for i in 0..max_sub_layers_minus1 {
        sub_profile[i] = r.flag()?;
        sub_level[i] = r.flag()?;
    }
    if max_sub_layers_minus1 > 0 {
        r.skip(2 * (8 - max_sub_layers_minus1)); // reserved_zero_2bits
    }
    for i in 0..max_sub_layers_minus1 {
        if sub_profile[i] {
            r.skip(88);
        }
        if sub_level[i] {
            r.skip(8);
        }
    }
    r.ue()?; // sps_seq_parameter_set_id
    let chroma = r.ue()?;
    if chroma > 3 {
        return Err(MediaError::Invalid("bad chroma_format_idc"));
    }
    let separate_planes = chroma == 3 && r.flag()?;
    let pic_width = u64::from(r.ue()?);
    let pic_height = u64::from(r.ue()?);
    let (mut left, mut right, mut top, mut bottom) = (0u64, 0u64, 0u64, 0u64);
    if r.flag()? {
        left = r.ue()?.into();
        right = r.ue()?.into();
        top = r.ue()?.into();
        bottom = r.ue()?.into();
    }
    let depth_luma = r.ue()? + 8;
    let depth_chroma = r.ue()? + 8;
    if depth_luma > 16 || depth_chroma > 16 {
        return Err(MediaError::Invalid("bad bit depth"));
    }
    let (sub_w, sub_h) = match (chroma, separate_planes) {
        (1, _) => (2, 2),
        (2, _) => (2, 1),
        _ => (1, 1),
    };
    let width = pic_width.checked_sub(sub_w * (left + right));
    let height = pic_height.checked_sub(sub_h * (top + bottom));
    let to_u32 = |v: Option<u64>| v.and_then(|v| u32::try_from(v).ok()).filter(|v| *v > 0);
    let (Some(width), Some(height)) = (to_u32(width), to_u32(height)) else {
        return Err(MediaError::Invalid("bad picture size"));
    };
    let mut constraint_flags = [0u8; 6];
    constraint_flags.copy_from_slice(&b[6..12]);
    Ok(HevcSps {
        profile_space: b[1] >> 6,
        high_tier: (b[1] >> 5) & 1 == 1,
        profile_idc: b[1] & 0x1f,
        compat_flags: u32::from_be_bytes([b[2], b[3], b[4], b[5]]),
        constraint_flags,
        level_idc: b[12],
        chroma_format_idc: chroma,
        bit_depth_luma: depth_luma as u8,
        bit_depth_chroma: depth_chroma as u8,
        width,
        height,
    })
}

/// Parse an SPS NAL unit (including its 2-byte NAL header, without start code).
pub fn parse_sps(nal: &[u8]) -> Result<HevcSps, MediaError> {
    let body = nal.get(2..).ok_or(MediaError::Truncated)?;
    parse_sps_rbsp(&unescape_rbsp_prefix(body, SPS_PREFIX_BYTES))
}

/// Codec string, bit depth and size from an HEVC config packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HevcInfo {
    /// RFC 6381 / ISO 14496-15 Annex E codec string, e.g. `hvc1.2.4.L123.90`.
    pub codec: String,
    /// Luma bit depth (8 when the SPS could not be parsed that far).
    pub bit_depth: u8,
    /// Display size in pixels, when the SPS could be parsed.
    pub dimensions: Option<(u32, u32)>,
}

/// Codec string (with the compatibility flags bit-reversed and trailing zero
/// constraint bytes trimmed), luma bit depth and size from the first SPS in an
/// Annex B buffer. Falls back to [`DEFAULT_CODEC`] / 8-bit without an SPS.
///
/// Some hardware encoders write 10-bit streams labelled as Main (8-bit);
/// decoders pick their pipeline from this label, so a Main stream with a luma
/// bit depth above 8 is reported as Main 10.
pub fn info(data: &[u8]) -> HevcInfo {
    for off in nal_offsets(data) {
        if (data[off] >> 1) & 0x3f != NAL_SPS {
            continue;
        }
        let b = unescape_rbsp_prefix(data.get(off + 2..).unwrap_or_default(), SPS_PREFIX_BYTES);
        if b.len() < 13 {
            break;
        }
        let space = usize::from(b[1] >> 6);
        let tier = (b[1] >> 5) & 1;
        let mut profile = b[1] & 0x1f;
        let mut reversed = u32::from_be_bytes([b[2], b[3], b[4], b[5]]).reverse_bits();
        let mut constraints = &b[6..12];
        while let [rest @ .., 0] = constraints {
            constraints = rest;
        }
        let cons: String = constraints.iter().map(|c| format!(".{c:X}")).collect();
        let sps = parse_sps_rbsp(&b).ok();
        let bit_depth = sps.map_or(8, |s| s.bit_depth_luma);
        if bit_depth > 8 && profile == 1 {
            profile = 2;
            reversed = 0x4;
        }
        let codec = format!(
            "hvc1.{}{}.{:X}.{}{}{}",
            ["", "A", "B", "C"][space],
            profile,
            reversed,
            if tier == 1 { 'H' } else { 'L' },
            b[12],
            cons
        );
        return HevcInfo {
            codec,
            bit_depth,
            dimensions: sps.map(|s| (s.width, s.height)),
        };
    }
    HevcInfo {
        codec: DEFAULT_CODEC.to_owned(),
        bit_depth: 8,
        dimensions: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_vectors::*;

    #[test]
    fn main10_x265() {
        let i = info(&hex(SPS_MAIN10_X265));
        assert_eq!(i.codec, "hvc1.2.4.L123.90");
        assert_eq!(i.bit_depth, 10);
        assert_eq!(i.dimensions, Some((1832, 960)));
    }

    #[test]
    fn mislabelled_10_bit_is_corrected_to_main10() {
        let i = info(&hex(SPS_10BIT_MISLABELLED_VAAPI));
        assert_eq!(i.bit_depth, 10);
        assert!(i.codec.starts_with("hvc1.2.4.H123"), "{}", i.codec);
        let sps = parse_sps(&hex(SPS_10BIT_MISLABELLED_VAAPI)[3..]).unwrap();
        assert_eq!(sps.profile_idc, 1);
        assert!(sps.high_tier);
    }

    #[test]
    fn main8_stays_main() {
        let i = info(&hex(SPS_MAIN8_VAAPI));
        assert_eq!(i.bit_depth, 8);
        assert!(i.codec.starts_with("hvc1.1.6.H123"), "{}", i.codec);
    }

    #[test]
    fn no_sps_gives_default() {
        assert_eq!(info(&[0, 0, 1, 0x40, 1, 0xc]).codec, DEFAULT_CODEC);
        // SPS header but too short to hold a profile_tier_level.
        let i = info(&[0, 0, 1, 0x42, 1, 1, 2, 3]);
        assert_eq!((i.codec.as_str(), i.bit_depth), (DEFAULT_CODEC, 8));
    }

    #[test]
    fn garbage_does_not_panic() {
        let mut seed = 0x9e37_79b9_u32;
        for len in 0..300 {
            let mut data = vec![0, 0, 1, 0x42, 0x01];
            data.extend((0..len).map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            }));
            let i = info(&data);
            assert!(i.codec.starts_with("hvc1."));
            let _ = parse_sps(&data[3..]);
        }
    }
}
