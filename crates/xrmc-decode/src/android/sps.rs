//! Picture size and bit depth from H.264 / H.265 SPS and AV1 sequence headers,
//! so MediaCodec can be configured before the first frame is decoded.

use crate::Codec;

/// What MediaCodec needs to know at configure time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamInfo {
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
}

struct Bits {
    data: Vec<u8>,
    pos: usize,
}

impl Bits {
    /// `rbsp`: strip emulation-prevention bytes (H.264/H.265 NAL units).
    fn new(data: &[u8], rbsp: bool) -> Bits {
        if !rbsp {
            return Bits {
                data: data.to_vec(),
                pos: 0,
            };
        }
        let mut out = Vec::with_capacity(data.len());
        let mut zeros = 0;
        for &b in data {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            out.push(b);
        }
        Bits { data: out, pos: 0 }
    }

    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        self.pos += 1;
        Some(u32::from((byte >> (7 - ((self.pos - 1) % 8))) & 1))
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }

    fn skip(&mut self, n: usize) -> Option<()> {
        self.pos += n;
        (self.pos <= self.data.len() * 8).then_some(())
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some(((1u64 << zeros) - 1 + u64::from(self.bits(zeros)?)) as u32)
    }

    fn se(&mut self) -> Option<i32> {
        let k = self.ue()?;
        Some(if k % 2 == 1 {
            k.div_ceil(2) as i32
        } else {
            -((k / 2) as i32)
        })
    }
}

/// Stream info from a packet that carries the parameter sets / sequence header.
pub fn parse(codec: Codec, data: &[u8]) -> Option<StreamInfo> {
    match codec {
        Codec::H264 => crate::stream::annexb_nals(data)
            .into_iter()
            .find(|n| n.first().is_some_and(|b| b & 0x1f == 7))
            .and_then(h264_sps),
        Codec::H265 => crate::stream::annexb_nals(data)
            .into_iter()
            .find(|n| n.first().is_some_and(|b| (b >> 1) & 0x3f == 33))
            .and_then(h265_sps),
        Codec::Av1 => av1_sequence_header(data),
    }
    .filter(|i| i.width > 0 && i.height > 0 && i.width <= 16384 && i.height <= 16384)
}

fn h264_sps(nal: &[u8]) -> Option<StreamInfo> {
    let mut r = Bits::new(nal, true);
    r.skip(8)?; // NAL header
    let profile = r.bits(8)?;
    r.skip(16)?; // constraint flags, level_idc
    r.ue()?; // seq_parameter_set_id
    let mut chroma = 1;
    let mut depth = 8;
    if [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].contains(&profile) {
        chroma = r.ue()?;
        if chroma == 3 {
            r.bit()?;
        }
        depth = 8 + r.ue()?;
        r.ue()?; // bit_depth_chroma_minus8
        r.bit()?; // qpprime_y_zero_transform_bypass_flag
        if r.bit()? == 1 {
            for i in 0..if chroma == 3 { 12 } else { 8 } {
                if r.bit()? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + r.se()? + 256).rem_euclid(256);
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    match r.ue()? {
        0 => {
            r.ue()?;
        }
        1 => {
            r.bit()?;
            r.se()?;
            r.se()?;
            let n = r.ue()?;
            if n > 255 {
                return None;
            }
            for _ in 0..n {
                r.se()?;
            }
        }
        _ => {}
    }
    r.ue()?; // max_num_ref_frames
    r.bit()?; // gaps_in_frame_num_value_allowed_flag
    let w_mbs = r.ue()? + 1;
    let h_units = r.ue()? + 1;
    let frame_mbs_only = r.bit()?;
    if frame_mbs_only == 0 {
        r.bit()?;
    }
    r.bit()?; // direct_8x8_inference_flag
    let (mut cl, mut cr, mut ct, mut cb) = (0, 0, 0, 0);
    if r.bit()? == 1 {
        cl = r.ue()?;
        cr = r.ue()?;
        ct = r.ue()?;
        cb = r.ue()?;
    }
    let (sub_w, sub_h) = match chroma {
        0 => (1, 1),
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let crop_x = sub_w;
    let crop_y = sub_h * (2 - frame_mbs_only);
    let width = (w_mbs * 16).checked_sub(crop_x * (cl + cr))?;
    let height = ((2 - frame_mbs_only) * h_units * 16).checked_sub(crop_y * (ct + cb))?;
    Some(StreamInfo {
        width,
        height,
        bit_depth: depth as u8,
    })
}

fn h265_sps(nal: &[u8]) -> Option<StreamInfo> {
    let mut r = Bits::new(nal, true);
    r.skip(16)?; // NAL header
    r.bits(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = r.bits(3)?;
    r.bit()?; // temporal_id_nesting
    // profile_tier_level(1, max_sub_layers_minus1)
    r.skip(96)?;
    let mut present = Vec::new();
    for _ in 0..max_sub_layers_minus1 {
        present.push((r.bit()?, r.bit()?));
    }
    if max_sub_layers_minus1 > 0 {
        r.skip(2 * (8 - max_sub_layers_minus1 as usize))?;
    }
    for (profile, level) in present {
        if profile == 1 {
            r.skip(88)?;
        }
        if level == 1 {
            r.skip(8)?;
        }
    }
    r.ue()?; // sps_seq_parameter_set_id
    let chroma = r.ue()?;
    if chroma == 3 {
        r.bit()?;
    }
    let mut width = r.ue()?;
    let mut height = r.ue()?;
    if r.bit()? == 1 {
        let (sub_w, sub_h) = match chroma {
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        };
        let (l, rr, t, b) = (r.ue()?, r.ue()?, r.ue()?, r.ue()?);
        width = width.checked_sub(sub_w * (l + rr))?;
        height = height.checked_sub(sub_h * (t + b))?;
    }
    let depth = 8 + r.ue()?;
    Some(StreamInfo {
        width,
        height,
        bit_depth: depth as u8,
    })
}

/// Find the sequence header OBU (also inside an `av1C` record) and parse it.
fn av1_sequence_header(data: &[u8]) -> Option<StreamInfo> {
    let mut d = if crate::stream::is_av1c(data) {
        &data[4..]
    } else {
        data
    };
    while !d.is_empty() {
        let header = d[0];
        let obu_type = (header >> 3) & 0x0f;
        let ext = usize::from(header & 0x04 != 0);
        let has_size = header & 0x02 != 0;
        let mut i = 1 + ext;
        let size = if has_size {
            let mut v = 0usize;
            let mut n = 0;
            loop {
                let b = *d.get(i + n)?;
                v |= usize::from(b & 0x7f) << (7 * n);
                n += 1;
                if b & 0x80 == 0 || n == 8 {
                    break;
                }
            }
            i += n;
            v
        } else {
            d.len().checked_sub(i)?
        };
        let payload = d.get(i..i.checked_add(size)?)?;
        if obu_type == 1 {
            return av1_seq(payload);
        }
        d = &d[i + size..];
    }
    None
}

fn av1_seq(p: &[u8]) -> Option<StreamInfo> {
    let mut r = Bits::new(p, false);
    let profile = r.bits(3)?;
    r.bit()?; // still_picture
    let reduced = r.bit()? == 1;
    let mut decoder_model_info = false;
    if reduced {
        r.bits(5)?; // seq_level_idx[0]
    } else {
        if r.bit()? == 1 {
            // timing_info
            r.skip(64)?;
            if r.bit()? == 1 {
                // num_ticks_per_picture_minus_1: uvlc
                let mut zeros = 0;
                while r.bit()? == 0 {
                    zeros += 1;
                    if zeros >= 32 {
                        return None;
                    }
                }
                r.skip(zeros)?;
            }
            decoder_model_info = r.bit()? == 1;
        }
        let mut buffer_delay_len = 0;
        if decoder_model_info {
            buffer_delay_len = r.bits(5)? + 1;
            r.skip(32 + 5 + 5)?;
        }
        let initial_display_delay = r.bit()? == 1;
        let ops = r.bits(5)? + 1;
        for _ in 0..ops {
            r.skip(12)?; // operating_point_idc
            let level = r.bits(5)?;
            if level > 7 {
                r.bit()?;
            }
            if decoder_model_info && r.bit()? == 1 {
                r.skip(2 * buffer_delay_len as usize + 1)?;
            }
            if initial_display_delay && r.bit()? == 1 {
                r.skip(4)?;
            }
        }
    }
    let wbits = r.bits(4)? + 1;
    let hbits = r.bits(4)? + 1;
    let width = r.bits(wbits)? + 1;
    let height = r.bits(hbits)? + 1;
    let mut frame_id = false;
    if !reduced {
        frame_id = r.bit()? == 1;
    }
    if frame_id {
        r.skip(7)?;
    }
    r.skip(3)?; // 128x128 superblock, filter intra, intra edge filter
    if !reduced {
        r.skip(4)?; // interintra, masked compound, warped motion, dual filter
        let order_hint = r.bit()? == 1;
        if order_hint {
            r.skip(2)?; // jnt_comp, ref_frame_mvs
        }
        let force_sct = if r.bit()? == 1 { 2 } else { r.bit()? };
        if force_sct > 0 && r.bit()? == 0 {
            r.bit()?; // seq_force_integer_mv
        }
        if order_hint {
            r.skip(3)?;
        }
    }
    r.skip(3)?; // superres, cdef, restoration
    let high = r.bit()? == 1;
    let depth = if profile == 2 && high {
        if r.bit()? == 1 { 12 } else { 10 }
    } else if high {
        10
    } else {
        8
    };
    Some(StreamInfo {
        width,
        height,
        bit_depth: depth,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    fn info(w: u32, h: u32, d: u8) -> Option<StreamInfo> {
        Some(StreamInfo {
            width: w,
            height: h,
            bit_depth: d,
        })
    }

    #[test]
    fn recorded_streams() {
        assert_eq!(
            parse(Codec::H264, &data("emulator_1280x800.h264")[..4096]),
            info(1280, 800, 8)
        );
        assert_eq!(
            parse(Codec::H265, &data("emulator_1280x800.h265")[..4096]),
            info(1280, 800, 8)
        );
        let main10 = parse(Codec::H265, &data("h265_main10.h265")[..4096]).unwrap();
        assert_eq!(main10.bit_depth, 10);
        let main8 = parse(Codec::H265, &data("h265_main.h265")[..4096]).unwrap();
        assert_eq!((main8.width, main8.bit_depth), (main10.width, 8));
        let a8 = parse(Codec::Av1, &data("av1_8.obu")[..4096]).unwrap();
        let a10 = parse(Codec::Av1, &data("av1_10.obu")[..4096]).unwrap();
        assert_eq!((a8.bit_depth, a10.bit_depth), (8, 10));
        assert_eq!((a8.width, a8.height), (a10.width, a10.height));
        assert_eq!(
            parse(Codec::H264, &data("h264_baseline.h264")[..4096]),
            info(320, 240, 8)
        );
        assert_eq!((a8.width, a8.height, main8.height), (320, 240, 240));
        assert_eq!(parse(Codec::H264, &[0, 0, 1, 0x65, 1, 2]), None);
    }
}
