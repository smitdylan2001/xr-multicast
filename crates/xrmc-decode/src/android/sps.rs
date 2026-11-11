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

    fn pos(&self) -> usize {
        self.pos
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

// ---------------------------------------------------------------------------
// H.264 SPS rewrite for low latency

struct BitWriter {
    out: Vec<u8>,
    bits: usize,
}

impl BitWriter {
    fn new() -> BitWriter {
        BitWriter {
            out: Vec::new(),
            bits: 0,
        }
    }

    fn bit(&mut self, b: u32) {
        if self.bits.is_multiple_of(8) {
            self.out.push(0);
        }
        if b != 0 {
            let last = self.out.len() - 1;
            self.out[last] |= 0x80 >> (self.bits % 8);
        }
        self.bits += 1;
    }

    fn bits(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.bit((v >> i) & 1);
        }
    }

    fn ue(&mut self, v: u32) {
        let x = u64::from(v) + 1;
        let len = 64 - x.leading_zeros();
        self.bits(0, len - 1);
        for i in (0..len).rev() {
            self.bit(((x >> i) & 1) as u32);
        }
    }

    /// RBSP trailing bits: a one, then zeros to the byte boundary.
    fn finish(mut self) -> Vec<u8> {
        self.bit(1);
        self.out
    }
}

/// Skip `hrd_parameters()`.
fn skip_hrd(r: &mut Bits) -> Option<()> {
    let cpb = r.ue()? + 1;
    if cpb > 32 {
        return None;
    }
    r.skip(8)?; // bit_rate_scale, cpb_size_scale
    for _ in 0..cpb {
        r.ue()?;
        r.ue()?;
        r.bit()?;
    }
    r.skip(20) // four 5-bit length fields
}

/// Rewrite an H.264 SPS NAL unit (with its header byte, emulation-prevention
/// bytes included) so it declares `max_num_reorder_frames = 0` and a minimal
/// `max_dec_frame_buffering`: decoders then output each picture right away
/// instead of holding pictures back for reordering (scrcpy streams have no
/// B-frames). Returns `None` when the SPS can't be parsed (use it unchanged).
pub fn h264_low_latency_sps(nal: &[u8]) -> Option<Vec<u8>> {
    let l = h264_sps_layout(nal)?;
    if l.low_latency {
        return None;
    }
    // Copy the RBSP bits up to the change, then write the new tail.
    let mut w = BitWriter::new();
    match l.restriction_pos {
        None => {
            copy_bits(nal, l.vui_flag_pos, &mut w);
            w.bit(1); // vui_parameters_present_flag
            // aspect_ratio, overscan, video_signal_type, chroma_loc, timing, nal_hrd, vcl_hrd, pic_struct
            w.bits(0, 8);
        }
        Some(p) => copy_bits(nal, p, &mut w),
    }
    w.bit(1); // bitstream_restriction_flag
    w.bit(1); // motion_vectors_over_pic_boundaries_flag
    w.ue(2); // max_bytes_per_pic_denom
    w.ue(1); // max_bits_per_mb_denom
    w.ue(16); // log2_max_mv_length_horizontal
    w.ue(16); // log2_max_mv_length_vertical
    w.ue(0); // max_num_reorder_frames
    w.ue(l.max_num_ref_frames.max(1)); // max_dec_frame_buffering
    Some(escape(&w.finish()))
}

/// Copy the first `n` RBSP bits of a NAL unit.
fn copy_bits(nal: &[u8], n: usize, w: &mut BitWriter) {
    let mut c = Bits::new(nal, true);
    for _ in 0..n {
        w.bit(c.bit().unwrap_or(0));
    }
}

/// Where the VUI and its bitstream restriction are in an H.264 SPS (bit positions in the RBSP).
struct SpsLayout {
    vui_flag_pos: usize,
    /// Position of `bitstream_restriction_flag` (when there is a VUI).
    restriction_pos: Option<usize>,
    max_num_ref_frames: u32,
    /// Already declares no reordering and a minimal decoded picture buffer.
    low_latency: bool,
}

fn h264_sps_layout(nal: &[u8]) -> Option<SpsLayout> {
    if nal.first()? & 0x1f != 7 {
        return None;
    }
    let mut r = Bits::new(nal, true);
    r.skip(8)?; // NAL header
    let profile = r.bits(8)?;
    r.skip(16)?;
    r.ue()?;
    if [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].contains(&profile) {
        let chroma = r.ue()?;
        if chroma == 3 {
            r.bit()?;
        }
        r.ue()?;
        r.ue()?;
        r.bit()?;
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
    let max_num_ref_frames = r.ue()?;
    r.bit()?;
    r.ue()?;
    r.ue()?;
    if r.bit()? == 0 {
        r.bit()?;
    }
    r.bit()?;
    if r.bit()? == 1 {
        for _ in 0..4 {
            r.ue()?;
        }
    }
    let vui_flag_pos = r.pos();
    let mut layout = SpsLayout {
        vui_flag_pos,
        restriction_pos: None,
        max_num_ref_frames,
        low_latency: false,
    };
    if r.bit()? == 0 {
        return Some(layout);
    }
    if r.bit()? == 1 && r.bits(8)? == 255 {
        r.skip(32)?;
    }
    if r.bit()? == 1 {
        r.bit()?;
    }
    if r.bit()? == 1 {
        r.skip(4)?;
        if r.bit()? == 1 {
            r.skip(24)?;
        }
    }
    if r.bit()? == 1 {
        r.ue()?;
        r.ue()?;
    }
    if r.bit()? == 1 {
        r.skip(65)?; // num_units_in_tick, time_scale, fixed_frame_rate_flag
    }
    let nal_hrd = r.bit()? == 1;
    if nal_hrd {
        skip_hrd(&mut r)?;
    }
    let vcl_hrd = r.bit()? == 1;
    if vcl_hrd {
        skip_hrd(&mut r)?;
    }
    if nal_hrd || vcl_hrd {
        r.bit()?; // low_delay_hrd_flag
    }
    r.bit()?; // pic_struct_present_flag
    layout.restriction_pos = Some(r.pos());
    if r.bit()? == 1 {
        r.bit()?;
        for _ in 0..4 {
            r.ue()?;
        }
        let reorder = r.ue()?;
        let buffering = r.ue()?;
        layout.low_latency = reorder == 0 && buffering <= max_num_ref_frames.max(1);
    }
    Some(layout)
}

/// Insert emulation-prevention bytes (00 00 0x → 00 00 03 0x for x ≤ 3).
fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 4);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// Replace every SPS in an Annex B buffer with its low-latency version.
pub fn h264_low_latency(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() + 16);
    let mut changed = false;
    let mut i = 0;
    let mut last = 0;
    while i + 3 <= data.len() {
        if data[i..i + 3] != [0, 0, 1] {
            i += 1;
            continue;
        }
        let start = i + 3;
        let mut end = start;
        while end + 3 <= data.len() && data[end..end + 3] != [0, 0, 1] {
            end += 1;
        }
        let end = if end + 3 > data.len() {
            data.len()
        } else {
            end
        };
        // Trailing zeros belong to the next start code (00 00 00 01).
        let mut nal_end = end;
        while nal_end > start && end < data.len() && data[nal_end - 1] == 0 {
            nal_end -= 1;
        }
        if let Some(fixed) = h264_low_latency_sps(&data[start..nal_end]) {
            out.extend_from_slice(&data[last..start]);
            out.extend_from_slice(&fixed);
            last = nal_end;
            changed = true;
        }
        i = end;
    }
    if !changed {
        return None;
    }
    out.extend_from_slice(&data[last..]);
    Some(out)
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

    /// `max_num_reorder_frames` / `max_dec_frame_buffering` of an SPS NAL (if present).
    fn restriction(nal: &[u8]) -> Option<(u32, u32)> {
        // Reuse the rewriter's parser: an already-low-latency SPS is not rewritten.
        h264_low_latency_sps(nal).map_or(Some((0, 1)), |_| None)
    }

    /// The SPS without its VUI (like encoders that write none).
    fn strip_vui(nal: &[u8]) -> Vec<u8> {
        let l = h264_sps_layout(nal).unwrap();
        let mut w = BitWriter::new();
        copy_bits(nal, l.vui_flag_pos, &mut w);
        w.bit(0);
        escape(&w.finish())
    }

    /// Replace the SPSs of an Annex B stream.
    fn map_sps(d: &[u8], f: impl Fn(&[u8]) -> Vec<u8>) -> Vec<u8> {
        let mut out = Vec::new();
        for nal in crate::stream::annexb_nals(d) {
            out.extend_from_slice(&[0, 0, 0, 1]);
            if nal[0] & 0x1f == 7 {
                out.extend_from_slice(&f(nal));
            } else {
                out.extend_from_slice(nal);
            }
        }
        out
    }

    #[test]
    fn low_latency_sps_keeps_the_stream_decodable() {
        // Android's encoders (and x264 zerolatency) already declare no reordering: left alone.
        for name in [
            "emulator_1280x800.h264",
            "h264_baseline.h264",
            "h264_high.h264",
        ] {
            assert!(h264_low_latency(&data(name)).is_none(), "{name}");
        }
        // Without a VUI (rewritten with a minimal one), and with a VUI that lacks the restriction.
        let no_vui = |n: &[u8]| strip_vui(n);
        let vui_no_restriction = |n: &[u8]| {
            let l = h264_sps_layout(n).unwrap();
            let mut w = BitWriter::new();
            copy_bits(n, l.restriction_pos.unwrap(), &mut w);
            w.bit(0);
            escape(&w.finish())
        };
        for (name, d) in [
            (
                "baseline, no VUI",
                map_sps(&data("h264_baseline.h264"), no_vui),
            ),
            ("high, no VUI", map_sps(&data("h264_high.h264"), no_vui)),
            (
                "full range, VUI without restriction",
                map_sps(&data("h264_fullrange_bt709.h264"), vui_no_restriction),
            ),
            (
                "emulator, VUI without restriction",
                map_sps(&data("emulator_1280x800.h264"), vui_no_restriction),
            ),
        ] {
            let fixed = h264_low_latency(&d).unwrap_or_else(|| panic!("{name}: not rewritten"));
            assert_eq!(parse(Codec::H264, &fixed), parse(Codec::H264, &d), "{name}");
            // Colour signalling survives.
            assert_eq!(
                crate::stream::h264_sps_color(&fixed),
                crate::stream::h264_sps_color(&d),
                "{name}"
            );
            // Rewriting again is a no-op (the result declares no reordering).
            assert!(h264_low_latency(&fixed).is_none(), "{name}");
            let sps = crate::stream::annexb_nals(&fixed)
                .into_iter()
                .find(|n| n[0] & 0x1f == 7)
                .unwrap();
            assert_eq!(restriction(sps), Some((0, 1)));
            // The whole stream still decodes, picture for picture.
            let count = |bytes: &[u8]| {
                let mut dec = crate::create_with(
                    Codec::H264,
                    crate::DecoderOptions::default(),
                    crate::Backend::OpenH264,
                )
                .unwrap();
                let mut frames = 0;
                for au in split_access_units(bytes) {
                    frames += dec.push(&au, 0, true, false).map_or(0, |f| f.len());
                }
                frames
            };
            assert_eq!(count(&fixed), count(&d), "{name}");
            assert!(count(&fixed) > 0, "{name}");
        }
    }

    /// Access units: split before each SPS or slice with first_mb_in_slice = 0 (good enough here).
    fn split_access_units(d: &[u8]) -> Vec<Vec<u8>> {
        let mut aus: Vec<Vec<u8>> = Vec::new();
        let mut cur: Vec<u8> = Vec::new();
        let mut seen_slice = false;
        for nal in crate::stream::annexb_nals(d) {
            let t = nal[0] & 0x1f;
            let starts = t == 7
                || t == 9
                || ((t == 1 || t == 5) && nal.get(1).is_some_and(|b| b & 0x80 != 0));
            if starts && seen_slice {
                aus.push(std::mem::take(&mut cur));
                seen_slice = false;
            }
            if t == 1 || t == 5 {
                seen_slice = true;
            }
            cur.extend_from_slice(&[0, 0, 0, 1]);
            cur.extend_from_slice(nal);
        }
        if !cur.is_empty() {
            aus.push(cur);
        }
        aus
    }
}
