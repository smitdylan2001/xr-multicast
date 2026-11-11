//! AV1: codec strings from `av1C` records and sequence header OBUs.

use crate::MediaError;
use crate::bits::{BitReader, leb128};

/// Codec string used when nothing better is known (Main, level 5.1, 8-bit).
pub const DEFAULT_CODEC: &str = "av01.0.13M.08";
/// Codec string to probe 10-bit decoding (Main, level 5.1, 10-bit).
pub const MAIN10_CODEC: &str = "av01.0.13M.10";

/// OBU type of a sequence header.
pub const OBU_SEQUENCE_HEADER: u8 = 1;

/// Whether `data` looks like an AV1CodecConfigurationRecord (`av1C`):
/// marker bit + version 1, and at least the 4 fixed bytes.
pub fn is_av1c(data: &[u8]) -> bool {
    data.len() >= 4 && data[0] == 0x81
}

/// The fields of an AV1 stream that make up its codec string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Av1Params {
    /// `seq_profile` (0 Main, 1 High, 2 Professional).
    pub profile: u8,
    /// `seq_level_idx[0]` (e.g. 13 = level 5.1).
    pub level: u8,
    /// `seq_tier[0]` (true = High tier).
    pub high_tier: bool,
    /// Bit depth (8, 10 or 12).
    pub bit_depth: u8,
}

impl Av1Params {
    /// Short-form codec string, e.g. `av01.0.08M.10`.
    pub fn codec_string(&self) -> String {
        format!(
            "av01.{}.{:02}{}.{:02}",
            self.profile,
            self.level,
            if self.high_tier { 'H' } else { 'M' },
            self.bit_depth
        )
    }
}

/// Codec parameters from an `av1C` record (see [`is_av1c`]).
pub fn av1c_params(rec: &[u8]) -> Result<Av1Params, MediaError> {
    if !is_av1c(rec) {
        return Err(MediaError::Invalid("not an av1C record"));
    }
    let twelve = (rec[2] >> 5) & 1 == 1;
    let high = (rec[2] >> 6) & 1 == 1;
    Ok(Av1Params {
        profile: rec[1] >> 5,
        level: rec[1] & 0x1f,
        high_tier: rec[2] >> 7 == 1,
        bit_depth: if twelve {
            12
        } else if high {
            10
        } else {
            8
        },
    })
}

/// The parts of a sequence header OBU the app needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequenceHeader {
    /// Profile, level, tier and bit depth.
    pub params: Av1Params,
    /// `max_frame_width_minus_1 + 1`.
    pub max_width: u32,
    /// `max_frame_height_minus_1 + 1`.
    pub max_height: u32,
    /// `mono_chrome`.
    pub monochrome: bool,
}

/// Parse a sequence header OBU payload (AV1 spec 5.5).
pub fn parse_sequence_header(payload: &[u8]) -> Result<SequenceHeader, MediaError> {
    let mut r = BitReader::new(payload);
    let profile = r.bits(3)? as u8;
    r.skip(1); // still_picture
    let reduced = r.flag()?;
    let level;
    let mut high_tier = false;
    if reduced {
        level = r.bits(5)? as u8;
    } else {
        let mut decoder_model_info = false;
        let mut buffer_delay_length = 0;
        if r.flag()? {
            // timing_info()
            r.skip(64); // num_units_in_display_tick, time_scale
            if r.flag()? {
                r.uvlc()?; // num_ticks_per_picture_minus_1
            }
            decoder_model_info = r.flag()?;
            if decoder_model_info {
                buffer_delay_length = r.bits(5)? + 1;
                r.skip(32 + 5 + 5);
            }
        }
        let initial_display_delay = r.flag()?;
        let operating_points = r.bits(5)? + 1;
        let mut first = None;
        for _ in 0..operating_points {
            r.skip(12); // operating_point_idc
            let idx = r.bits(5)? as u8;
            let tier = idx > 7 && r.flag()?;
            first.get_or_insert((idx, tier));
            if decoder_model_info && r.flag()? {
                r.skip(2 * buffer_delay_length as usize + 1);
            }
            if initial_display_delay && r.flag()? {
                r.skip(4);
            }
        }
        (level, high_tier) = first.unwrap_or_default();
    }
    let width_bits = r.bits(4)? + 1;
    let height_bits = r.bits(4)? + 1;
    let max_width = r
        .bits(width_bits)?
        .checked_add(1)
        .ok_or(MediaError::Invalid("bad frame width"))?;
    let max_height = r
        .bits(height_bits)?
        .checked_add(1)
        .ok_or(MediaError::Invalid("bad frame height"))?;
    if !reduced && r.flag()? {
        r.skip(4 + 3); // delta_frame_id_length_minus_2, additional_frame_id_length_minus_1
    }
    r.skip(3); // use_128x128_superblock, enable_filter_intra, enable_intra_edge_filter
    if !reduced {
        r.skip(4); // interintra_compound, masked_compound, warped_motion, dual_filter
        let order_hint = r.flag()?;
        if order_hint {
            r.skip(2); // enable_jnt_comp, enable_ref_frame_mvs
        }
        let force_screen_content_tools = if r.flag()? { 2 } else { r.bits(1)? };
        if force_screen_content_tools > 0 && !r.flag()? {
            r.skip(1); // seq_force_integer_mv
        }
        if order_hint {
            r.skip(3); // order_hint_bits_minus_1
        }
    }
    r.skip(3); // enable_superres, enable_cdef, enable_restoration
    // color_config()
    let high_bitdepth = r.flag()?;
    let bit_depth = if profile == 2 && high_bitdepth {
        if r.flag()? { 12 } else { 10 }
    } else if high_bitdepth {
        10
    } else {
        8
    };
    let monochrome = profile != 1 && r.flag()?;
    Ok(SequenceHeader {
        params: Av1Params {
            profile,
            level,
            high_tier,
            bit_depth,
        },
        max_width,
        max_height,
        monochrome,
    })
}

/// Find and parse the first sequence header in a low-overhead OBU stream
/// (e.g. a config packet of raw OBUs, or the configOBUs of an `av1C`).
pub fn find_sequence_header(mut data: &[u8]) -> Result<SequenceHeader, MediaError> {
    while let Some(&header) = data.first() {
        if header & 0x80 != 0 {
            return Err(MediaError::Invalid("OBU forbidden bit set"));
        }
        let obu_type = (header >> 3) & 0x0f;
        let has_extension = (header >> 2) & 1 == 1;
        let has_size = (header >> 1) & 1 == 1;
        let mut pos = 1 + usize::from(has_extension);
        let size = if has_size {
            let (size, n) = leb128(data.get(pos..).ok_or(MediaError::Truncated)?)?;
            pos += n;
            usize::try_from(size).map_err(|_| MediaError::Truncated)?
        } else {
            data.len().saturating_sub(pos)
        };
        let end = pos
            .checked_add(size)
            .filter(|e| *e <= data.len())
            .ok_or(MediaError::Truncated)?;
        if obu_type == OBU_SEQUENCE_HEADER {
            return parse_sequence_header(&data[pos..end]);
        }
        data = &data[end..];
    }
    Err(MediaError::Invalid("no sequence header OBU"))
}

#[cfg(test)]
pub(crate) mod test_util {
    use crate::h264::test_util::BitWriter;

    /// A minimal non-reduced sequence header OBU (with size field) for a
    /// `width` x `height` stream.
    pub fn sequence_header_obu(
        profile: u8,
        level: u8,
        tier: bool,
        ten_bit: bool,
        width: u32,
        height: u32,
    ) -> Vec<u8> {
        let mut w = BitWriter::default()
            .u(3, profile.into())
            .u(1, 0) // still_picture
            .u(1, 0) // reduced_still_picture_header
            .u(1, 0) // timing_info_present_flag
            .u(1, 0) // initial_display_delay_present_flag
            .u(5, 0) // operating_points_cnt_minus_1
            .u(12, 0)
            .u(5, level.into());
        if level > 7 {
            w = w.u(1, tier.into());
        }
        w = w
            .u(4, 15)
            .u(4, 15)
            .u(16, (width - 1).into())
            .u(16, (height - 1).into())
            .u(1, 0) // frame_id_numbers_present_flag
            .u(3, 0b011)
            .u(4, 0)
            .u(1, 1) // enable_order_hint
            .u(2, 0)
            .u(1, 1) // seq_choose_screen_content_tools
            .u(1, 1) // seq_choose_integer_mv
            .u(3, 6) // order_hint_bits_minus_1
            .u(3, 0b010)
            .u(1, ten_bit.into()); // high_bitdepth
        if profile != 1 {
            w = w.u(1, 0); // mono_chrome
        }
        // rest of color_config + film grain: irrelevant here
        let payload = w.u(4, 0).bytes();
        let mut obu = vec![0x0a, payload.len() as u8];
        obu.extend_from_slice(&payload);
        obu
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::sequence_header_obu;
    use super::*;

    #[test]
    fn av1c_record() {
        let p = av1c_params(&[0x81, 0x08, 0x4c, 0x00]).unwrap();
        assert_eq!(p.codec_string(), "av01.0.08M.10");
        assert_eq!(p.bit_depth, 10);
        let p = av1c_params(&[0x81, 0x4d, 0xa0, 0x00]).unwrap();
        assert_eq!(p.codec_string(), "av01.2.13H.12");
        assert!(av1c_params(&[0x81, 0, 0]).is_err());
    }

    #[test]
    fn sequence_header_from_obus() {
        // temporal delimiter, then the sequence header
        let mut data = vec![0x12, 0x00];
        data.extend(sequence_header_obu(0, 13, false, true, 3664, 1920));
        let sh = find_sequence_header(&data).unwrap();
        assert_eq!(sh.params.codec_string(), "av01.0.13M.10");
        assert_eq!((sh.max_width, sh.max_height), (3664, 1920));
        let sh = find_sequence_header(&sequence_header_obu(0, 9, true, false, 1280, 720)).unwrap();
        assert_eq!(sh.params.codec_string(), "av01.0.09H.08");
        assert!(!sh.monochrome);
    }

    #[test]
    fn garbage_does_not_panic() {
        let mut seed = 0xdead_beef_u32;
        for len in 0..200 {
            let data: Vec<u8> = (0..len)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    seed as u8
                })
                .collect();
            let _ = find_sequence_header(&data);
            let _ = parse_sequence_header(&data);
            let _ = av1c_params(&data);
        }
        assert!(find_sequence_header(&[0x0a, 0x7f, 0x00]).is_err());
        assert!(find_sequence_header(&[0x0a, 0x80]).is_err());
    }
}
