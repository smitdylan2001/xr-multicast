//! Backend-independent handling of scrcpy's packet stream (same semantics as the
//! JS `StreamDecoder` in `src/renderer/js/decoder.js`):
//!
//! - A config packet (SPS/PPS[/VPS] for H.264/H.265; an `av1C` record or raw
//!   sequence-header OBUs for AV1) is remembered and prepended to key frames.
//! - Before the first key frame (and after a config change or a decoder error),
//!   delta frames are dropped until the next key frame.

use std::borrow::Cow;

use crate::Codec;

/// What to feed the decoder for one packet.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Prepared<'a> {
    /// Nothing to decode (config packet, or a delta frame while waiting for a key frame).
    Skip,
    /// Decode these bytes (one access unit / temporal unit).
    Feed(Cow<'a, [u8]>),
}

#[derive(Debug)]
pub(crate) struct StreamState {
    codec: Codec,
    /// Bytes inserted into key frames (Annex B parameter sets or AV1 config OBUs).
    prefix: Option<Vec<u8>>,
    wait_key: bool,
    dropped: u64,
}

impl StreamState {
    pub fn new(codec: Codec) -> StreamState {
        StreamState {
            codec,
            prefix: None,
            wait_key: true,
            dropped: 0,
        }
    }

    pub fn prepare<'a>(&mut self, data: &'a [u8], key: bool, config: bool) -> Prepared<'a> {
        if config {
            self.prefix = match self.codec {
                Codec::H264 | Codec::H265 => Some(data.to_vec()),
                Codec::Av1 => av1_config_obus(data),
            }
            .filter(|p| !p.is_empty());
            self.wait_key = true;
            return Prepared::Skip;
        }
        if !key && self.wait_key {
            self.dropped += 1;
            return Prepared::Skip;
        }
        if data.is_empty() {
            return Prepared::Skip;
        }
        match (&self.prefix, key) {
            (Some(prefix), true) => Prepared::Feed(Cow::Owned(match self.codec {
                Codec::H264 | Codec::H265 => [prefix.as_slice(), data].concat(),
                Codec::Av1 => av1_insert_config(prefix, data),
            })),
            _ => Prepared::Feed(Cow::Borrowed(data)),
        }
    }

    /// The packet was accepted by the decoder.
    pub fn fed(&mut self) {
        self.wait_key = false;
    }

    /// The decoder failed: resynchronise on the next key frame.
    pub fn on_error(&mut self) {
        self.wait_key = true;
    }

    pub fn waiting_for_key(&self) -> bool {
        self.wait_key
    }

    #[allow(dead_code)]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn prefix(&self) -> Option<&[u8]> {
        self.prefix.as_deref()
    }
}

/// BT.709 for HD pictures (shorter side ≥ 720), BT.601 below.
pub(crate) fn default_bt709(w: u32, h: u32) -> bool {
    w.min(h) >= 720
}

// ---------------------------------------------------------------------------
// AV1

const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_TEMPORAL_DELIMITER: u8 = 2;

/// One OBU in a low-overhead bitstream: (type, total length including header).
fn next_obu(data: &[u8]) -> Option<(u8, usize)> {
    let header = *data.first()?;
    let obu_type = (header >> 3) & 0x0f;
    let ext = header & 0x04 != 0;
    let has_size = header & 0x02 != 0;
    let hdr_len = 1 + usize::from(ext);
    if data.len() < hdr_len {
        return None;
    }
    if !has_size {
        return Some((obu_type, data.len()));
    }
    let (size, n) = leb128(&data[hdr_len..])?;
    let total = hdr_len
        .checked_add(n)?
        .checked_add(usize::try_from(size).ok()?)?;
    (total <= data.len()).then_some((obu_type, total))
}

fn leb128(data: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (i, &b) in data.iter().take(8).enumerate() {
        value |= u64::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// Split a low-overhead OBU stream into (type, bytes). Stops at the first malformed OBU.
fn obus(mut data: &[u8]) -> Vec<(u8, &[u8])> {
    let mut out = Vec::new();
    while let Some((t, len)) = next_obu(data) {
        out.push((t, &data[..len]));
        data = &data[len..];
    }
    out
}

/// Whether `data` is an `AV1CodecConfigurationRecord` (marker bit + version 1).
pub(crate) fn is_av1c(data: &[u8]) -> bool {
    data.len() >= 4 && data[0] == 0x81
}

/// The OBUs to insert into key frames from an AV1 config packet: the av1C's
/// `configOBUs`, or the packet itself when it carries raw OBUs. Temporal
/// delimiters are dropped (the key frame has its own).
fn av1_config_obus(data: &[u8]) -> Option<Vec<u8>> {
    let raw = if is_av1c(data) { &data[4..] } else { data };
    let parsed = obus(raw);
    if parsed.iter().map(|(_, b)| b.len()).sum::<usize>() != raw.len() {
        // Not a clean OBU sequence: pass it through untouched.
        return Some(raw.to_vec());
    }
    Some(
        parsed
            .into_iter()
            .filter(|(t, _)| *t != OBU_TEMPORAL_DELIMITER)
            .flat_map(|(_, b)| b.iter().copied())
            .collect(),
    )
}

/// Insert config OBUs into a key temporal unit (after its temporal delimiter),
/// unless it already carries a sequence header.
fn av1_insert_config(prefix: &[u8], data: &[u8]) -> Vec<u8> {
    let parsed = obus(data);
    if parsed.iter().any(|(t, _)| *t == OBU_SEQUENCE_HEADER) {
        return data.to_vec();
    }
    let td_len = match parsed.first() {
        Some((OBU_TEMPORAL_DELIMITER, b)) => b.len(),
        _ => 0,
    };
    [&data[..td_len], prefix, &data[td_len..]].concat()
}

// ---------------------------------------------------------------------------
// H.264 SPS colour description (used by the OpenH264 backend, which doesn't report it)

/// Colour signalling from an H.264 SPS VUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct SpsColor {
    pub full_range: Option<bool>,
    /// `matrix_coefficients` (1 = BT.709, 5/6 = BT.601, 9/10 = BT.2020).
    pub matrix: Option<u8>,
}

struct Bits {
    data: Vec<u8>,
    pos: usize,
}

impl Bits {
    fn new(nal: &[u8]) -> Bits {
        // Strip emulation-prevention bytes (00 00 03 -> 00 00).
        let mut data = Vec::with_capacity(nal.len());
        let mut zeros = 0;
        for &b in nal {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            data.push(b);
        }
        Bits { data, pos: 0 }
    }

    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let v = (byte >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Some(u32::from(v))
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
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

/// NAL unit payloads (after the start code) in an Annex B buffer.
pub(crate) fn annexb_nals(data: &[u8]) -> Vec<&[u8]> {
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
    let mut out = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let mut end = starts.get(k + 1).map_or(data.len(), |&n| n - 3);
        while end > s && data[end - 1] == 0 {
            end -= 1;
        }
        out.push(&data[s..end]);
    }
    out
}

/// Colour info from the first SPS in an H.264 Annex B buffer.
pub(crate) fn h264_sps_color(data: &[u8]) -> Option<SpsColor> {
    let sps = annexb_nals(data)
        .into_iter()
        .find(|n| n.first().is_some_and(|b| b & 0x1f == 7))?;
    parse_h264_sps_color(sps)
}

fn parse_h264_sps_color(nal: &[u8]) -> Option<SpsColor> {
    let mut r = Bits::new(nal);
    r.bits(8)?; // NAL header
    let profile = r.bits(8)?;
    r.bits(16)?; // constraint flags, level_idc
    r.ue()?; // seq_parameter_set_id
    if [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].contains(&profile) {
        let chroma = r.ue()?;
        if chroma == 3 {
            r.bit()?;
        }
        r.ue()?; // bit_depth_luma_minus8
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
    r.ue()?; // pic_width_in_mbs_minus1
    r.ue()?; // pic_height_in_map_units_minus1
    if r.bit()? == 0 {
        r.bit()?; // mb_adaptive_frame_field_flag
    }
    r.bit()?; // direct_8x8_inference_flag
    if r.bit()? == 1 {
        for _ in 0..4 {
            r.ue()?;
        }
    }
    if r.bit()? == 0 {
        return Some(SpsColor::default()); // no VUI
    }
    if r.bit()? == 1 && r.bits(8)? == 255 {
        r.bits(32)?; // sar_width, sar_height
    }
    if r.bit()? == 1 {
        r.bit()?; // overscan_appropriate_flag
    }
    if r.bit()? == 0 {
        return Some(SpsColor::default()); // no video_signal_type
    }
    r.bits(3)?; // video_format
    let full_range = r.bit()? == 1;
    let matrix = if r.bit()? == 1 {
        r.bits(16)?; // colour_primaries, transfer_characteristics
        Some(r.bits(8)? as u8)
    } else {
        None
    };
    Some(SpsColor {
        full_range: Some(full_range),
        matrix,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(p: Prepared<'_>) -> Option<Vec<u8>> {
        match p {
            Prepared::Skip => None,
            Prepared::Feed(b) => Some(b.into_owned()),
        }
    }

    #[test]
    fn drops_deltas_until_key_and_prepends_config() {
        let mut s = StreamState::new(Codec::H264);
        assert_eq!(feed(s.prepare(&[0, 0, 0, 1, 0x41, 1], false, false)), None);
        assert_eq!(feed(s.prepare(&[0, 0, 0, 1, 0x67, 9], false, true)), None);
        assert_eq!(feed(s.prepare(&[0, 0, 0, 1, 0x41, 2], false, false)), None);
        assert_eq!(s.dropped(), 2);
        let key = feed(s.prepare(&[0, 0, 0, 1, 0x65, 3], true, false)).unwrap();
        assert_eq!(key, [0, 0, 0, 1, 0x67, 9, 0, 0, 0, 1, 0x65, 3]);
        s.fed();
        assert_eq!(
            feed(s.prepare(&[0, 0, 0, 1, 0x41, 4], false, false)).unwrap(),
            [0, 0, 0, 1, 0x41, 4]
        );
        s.on_error();
        assert!(s.waiting_for_key());
        assert_eq!(feed(s.prepare(&[0, 0, 0, 1, 0x41, 5], false, false)), None);
    }

    #[test]
    fn key_frame_without_config_is_fed() {
        let mut s = StreamState::new(Codec::H265);
        assert_eq!(feed(s.prepare(&[1, 2, 3], true, false)).unwrap(), [1, 2, 3]);
    }

    // OBUs: TD = 0x12 0x00; seq header type 1 = 0x0a <size> ...; frame type 6 = 0x32 <size> ...
    const TD: [u8; 2] = [0x12, 0x00];
    const SEQ: [u8; 4] = [0x0a, 0x02, 0xaa, 0xbb];
    const FRAME: [u8; 5] = [0x32, 0x03, 1, 2, 3];

    #[test]
    fn av1c_config_obus_go_after_temporal_delimiter() {
        let mut s = StreamState::new(Codec::Av1);
        let av1c = [&[0x81, 0x08, 0x0c, 0x00][..], &SEQ].concat();
        assert_eq!(feed(s.prepare(&av1c, false, true)), None);
        assert_eq!(s.prefix().unwrap(), SEQ);
        let key = [&TD[..], &FRAME].concat();
        assert_eq!(
            feed(s.prepare(&key, true, false)).unwrap(),
            [&TD[..], &SEQ, &FRAME].concat()
        );
        // Already has a sequence header: unchanged.
        let key2 = [&TD[..], &SEQ, &FRAME].concat();
        assert_eq!(feed(s.prepare(&key2, true, false)).unwrap(), key2);
        // No temporal delimiter: prepended.
        assert_eq!(
            feed(s.prepare(&FRAME, true, false)).unwrap(),
            [&SEQ[..], &FRAME].concat()
        );
    }

    #[test]
    fn av1c_without_config_obus_and_raw_obu_config() {
        let mut s = StreamState::new(Codec::Av1);
        s.prepare(&[0x81, 0x08, 0x0c, 0x00], false, true);
        assert_eq!(s.prefix(), None);
        s.prepare(&[&TD[..], &SEQ].concat(), false, true);
        assert_eq!(s.prefix().unwrap(), SEQ);
    }

    #[test]
    fn leb128_and_obu_parsing() {
        assert_eq!(leb128(&[0x80, 0x01]), Some((128, 2)));
        assert_eq!(next_obu(&[0x32, 0x05, 1]), None); // truncated
        assert_eq!(obus(&[&TD[..], &SEQ, &FRAME].concat()).len(), 3);
    }

    #[test]
    fn sps_color_parsing() {
        // x264 (tests/data/h264_baseline.h264): VUI with video_signal_type, limited range, BT.601 (SMPTE 170M) matrix.
        let x264 = [
            0x67, 0x42, 0xc0, 0x0d, 0xd9, 0x01, 0x41, 0xfb, 0x01, 0x6a, 0x0c, 0x0c, 0x0d, 0x4a,
            0x00, 0x00, 0x03, 0x00, 0x02, 0x00, 0x00, 0x03, 0x00, 0x79, 0x1e, 0x28, 0x54, 0x90,
        ];
        assert_eq!(
            parse_h264_sps_color(&x264),
            Some(SpsColor {
                full_range: Some(false),
                matrix: Some(6)
            })
        );
        // x264 High profile.
        let high = [
            0x67, 0x64, 0x00, 0x0d, 0xac, 0xb2, 0x02, 0x83, 0xf6, 0x02, 0xd4, 0x18, 0x18, 0x1a,
            0x94, 0x00, 0x00, 0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xf2, 0x3c, 0x50, 0xa9,
            0x20,
        ];
        assert_eq!(
            parse_h264_sps_color(&high),
            Some(SpsColor {
                full_range: Some(false),
                matrix: Some(6)
            })
        );
        // Android emulator encoder (tests/data/emulator_1280x800.h264).
        let emu = [
            0x67, 0x42, 0xc0, 0x29, 0x8d, 0x68, 0x05, 0x00, 0x65, 0xa4, 0x20, 0x20, 0x20, 0x20,
            0xf0, 0x88, 0x46, 0xa0,
        ];
        let c = parse_h264_sps_color(&emu).unwrap();
        println!("emulator SPS colour: {c:?}");
    }

    #[test]
    fn annexb_split() {
        let d = [0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 3];
        assert_eq!(
            annexb_nals(&d),
            vec![&[0x67, 1][..], &[0x68, 2], &[0x65, 3]]
        );
    }

    #[test]
    fn hd_heuristic() {
        assert!(default_bt709(1280, 720));
        assert!(default_bt709(720, 1280));
        assert!(!default_bt709(960, 540));
    }
}
