//! H.264 Annex B -> fragmented MP4 (for Media Source Extensions and other
//! fMP4 consumers).
//!
//! Byte-for-byte port of the `Fmp4` helpers in the Electron app's
//! `mse-decoder.js`: one `ftyp`+`moov` init segment (with `avcC`), then one
//! `moof`+`mdat` fragment per frame holding a single AVCC sample.
//!
//! [`Fmp4Muxer`] adds the stream logic of the JS `MseDecoder.push`: it waits
//! for a key frame, rebuilds the init segment when the stream changes, and
//! keeps a synthetic, contiguous timeline (each frame lasts until the next one
//! is expected) because scrcpy only sends frames when the screen changes: real
//! pts gaps would leave holes in the buffer that stall playback.

use crate::MediaError;
use crate::h264::{AvcConfig, nal_type, parse_config};
use crate::nal::split_nals;

/// Media timescale of the video track (ticks per second).
pub const TIMESCALE: u32 = 90_000;

/// `trun` sample flags of a sync sample (sample_depends_on = 2).
pub const KEY_FLAGS: u32 = 0x0200_0000;
/// `trun` sample flags of a non-sync sample (depends_on = 1, is_non_sync).
pub const DELTA_FLAGS: u32 = 0x0101_0000;

const MATRIX: [u32; 9] = [0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x4000_0000];

/// Builds one MP4 box in place.
struct BoxWriter {
    buf: Vec<u8>,
}

impl BoxWriter {
    fn new(kind: &[u8; 4]) -> Self {
        let mut buf = Vec::with_capacity(64);
        buf.extend_from_slice(&[0, 0, 0, 0]);
        buf.extend_from_slice(kind);
        Self { buf }
    }

    fn full(kind: &[u8; 4], version: u8, flags: u32) -> Self {
        let mut b = Self::new(kind);
        b.buf.push(version);
        b.buf.extend_from_slice(&flags.to_be_bytes()[1..]);
        b
    }

    fn u16(mut self, v: u16) -> Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    fn u32(mut self, v: u32) -> Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    fn u64(mut self, v: u64) -> Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    fn zeros(mut self, n: usize) -> Self {
        self.buf.resize(self.buf.len() + n, 0);
        self
    }

    fn bytes(mut self, v: &[u8]) -> Self {
        self.buf.extend_from_slice(v);
        self
    }

    fn matrix(self) -> Self {
        MATRIX.iter().fold(self, |b, &v| b.u32(v))
    }

    fn finish(mut self) -> Vec<u8> {
        let len = self.buf.len() as u32;
        self.buf[..4].copy_from_slice(&len.to_be_bytes());
        self.buf
    }
}

/// The `avcC` box for a parsed config.
pub fn avcc_box(cfg: &AvcConfig) -> Vec<u8> {
    BoxWriter::new(b"avcC")
        .bytes(&cfg.decoder_configuration_record())
        .finish()
}

/// `ftyp` + `moov` init segment for a single H.264 video track (track id 1,
/// timescale [`TIMESCALE`]), sized `cfg.width() x cfg.height()`.
pub fn init_segment(cfg: &AvcConfig) -> Vec<u8> {
    let (width, height) = (cfg.width(), cfg.height());
    let ftyp = BoxWriter::new(b"ftyp")
        .bytes(b"isom")
        .u32(0x200)
        .bytes(b"isomiso6avc1mp41")
        .finish();
    let mvhd = BoxWriter::full(b"mvhd", 0, 0)
        .u32(0)
        .u32(0)
        .u32(1000)
        .u32(0)
        .u32(0x10000)
        .u16(0x100)
        .zeros(10)
        .matrix()
        .zeros(24)
        .u32(2)
        .finish();
    let tkhd = BoxWriter::full(b"tkhd", 0, 3)
        .u32(0)
        .u32(0)
        .u32(1)
        .u32(0)
        .u32(0)
        .zeros(8)
        .u16(0)
        .u16(0)
        .u16(0)
        .u16(0)
        .matrix()
        .u32(width << 16)
        .u32(height << 16)
        .finish();
    let mdhd = BoxWriter::full(b"mdhd", 0, 0)
        .u32(0)
        .u32(0)
        .u32(TIMESCALE)
        .u32(0)
        .u16(0x55c4)
        .u16(0)
        .finish();
    let hdlr = BoxWriter::full(b"hdlr", 0, 0)
        .u32(0)
        .bytes(b"vide")
        .zeros(12)
        .bytes(b"VideoHandler\0")
        .finish();
    let vmhd = BoxWriter::full(b"vmhd", 0, 1).zeros(8).finish();
    let url = BoxWriter::full(b"url ", 0, 1).finish();
    let dref = BoxWriter::full(b"dref", 0, 0).u32(1).bytes(&url).finish();
    let dinf = BoxWriter::new(b"dinf").bytes(&dref).finish();
    let avc1 = BoxWriter::new(b"avc1")
        .zeros(6)
        .u16(1)
        .zeros(16)
        .u16(width as u16)
        .u16(height as u16)
        .u32(0x48_0000)
        .u32(0x48_0000)
        .u32(0)
        .u16(1)
        .zeros(32)
        .u16(0x18)
        .u16(0xffff)
        .bytes(&avcc_box(cfg))
        .finish();
    let stsd = BoxWriter::full(b"stsd", 0, 0).u32(1).bytes(&avc1).finish();
    let stbl = BoxWriter::new(b"stbl")
        .bytes(&stsd)
        .bytes(&BoxWriter::full(b"stts", 0, 0).u32(0).finish())
        .bytes(&BoxWriter::full(b"stsc", 0, 0).u32(0).finish())
        .bytes(&BoxWriter::full(b"stsz", 0, 0).u32(0).u32(0).finish())
        .bytes(&BoxWriter::full(b"stco", 0, 0).u32(0).finish())
        .finish();
    let minf = BoxWriter::new(b"minf")
        .bytes(&vmhd)
        .bytes(&dinf)
        .bytes(&stbl)
        .finish();
    let mdia = BoxWriter::new(b"mdia")
        .bytes(&mdhd)
        .bytes(&hdlr)
        .bytes(&minf)
        .finish();
    let trak = BoxWriter::new(b"trak").bytes(&tkhd).bytes(&mdia).finish();
    let trex = BoxWriter::full(b"trex", 0, 0)
        .u32(1)
        .u32(1)
        .u32(0)
        .u32(0)
        .u32(0)
        .finish();
    let mvex = BoxWriter::new(b"mvex").bytes(&trex).finish();
    let moov = BoxWriter::new(b"moov")
        .bytes(&mvhd)
        .bytes(&trak)
        .bytes(&mvex)
        .finish();
    let mut out = ftyp;
    out.extend_from_slice(&moov);
    out
}

/// Annex B access unit -> AVCC sample (4-byte length-prefixed NAL units),
/// dropping parameter sets and access unit delimiters (they live in `avcC`).
pub fn to_avcc_sample(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 8);
    for nal in split_nals(data) {
        let Some(&header) = nal.first() else { continue };
        if matches!(header & 0x1f, nal_type::SPS | nal_type::PPS | nal_type::AUD) {
            continue;
        }
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

/// One `moof`+`mdat` fragment holding a single sample.
///
/// `base_time` (in [`TIMESCALE`] units) goes into a version 1 (64-bit) `tfdt`.
pub fn fragment(seq: u32, base_time: u64, duration: u32, sample: &[u8], is_key: bool) -> Vec<u8> {
    let tfhd = BoxWriter::full(b"tfhd", 0, 0x020000).u32(1).finish();
    let tfdt = BoxWriter::full(b"tfdt", 1, 0).u64(base_time).finish();
    const TRUN_SIZE: usize = 8 + 4 + 4 + 4 + 12;
    // moof header(8) + mfhd(16) + traf header(8) + tfhd + tfdt + trun + mdat header(8)
    let data_offset = 8 + 16 + 8 + tfhd.len() + tfdt.len() + TRUN_SIZE + 8;
    let trun = BoxWriter::full(b"trun", 0, 0x000701)
        .u32(1)
        .u32(data_offset as u32)
        .u32(duration)
        .u32(sample.len() as u32)
        .u32(if is_key { KEY_FLAGS } else { DELTA_FLAGS })
        .finish();
    let mfhd = BoxWriter::full(b"mfhd", 0, 0).u32(seq).finish();
    let traf = BoxWriter::new(b"traf")
        .bytes(&tfhd)
        .bytes(&tfdt)
        .bytes(&trun)
        .finish();
    let moof = BoxWriter::new(b"moof").bytes(&mfhd).bytes(&traf).finish();
    let mut out = Vec::with_capacity(moof.len() + 8 + sample.len());
    out.extend_from_slice(&moof);
    out.extend_from_slice(&(sample.len() as u32 + 8).to_be_bytes());
    out.extend_from_slice(b"mdat");
    out.extend_from_slice(sample);
    out
}

/// What one [`Fmp4Muxer::push`] produced. Append `init_segment` (if any)
/// before `media_segment`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MuxOutput {
    /// A new `ftyp`+`moov`: the stream (re)started with new parameters. The
    /// consumer should reset its playback pipeline (e.g. a new SourceBuffer).
    pub init_segment: Option<Vec<u8>>,
    /// A `moof`+`mdat` fragment for the pushed frame.
    pub media_segment: Option<Vec<u8>>,
}

/// Stateful H.264 -> fMP4 remuxer for a scrcpy packet stream.
#[derive(Debug, Clone)]
pub struct Fmp4Muxer {
    cfg: Option<AvcConfig>,
    wait_key: bool,
    seq: u32,
    time: u64,
    last_pts: Option<u64>,
    dropped: u64,
}

impl Default for Fmp4Muxer {
    fn default() -> Self {
        Self::new()
    }
}

impl Fmp4Muxer {
    /// A muxer waiting for a config packet (or a key frame with in-band SPS/PPS).
    pub fn new() -> Self {
        Self {
            cfg: None,
            wait_key: true,
            seq: 1,
            time: 0,
            last_pts: None,
            dropped: 0,
        }
    }

    /// The current stream parameters (codec string for the MSE mime type, size).
    pub fn config(&self) -> Option<&AvcConfig> {
        self.cfg.as_ref()
    }

    /// Frames dropped while waiting for a key frame.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Restart at the next key frame (e.g. after the consumer lost data).
    pub fn request_key_frame(&mut self) {
        self.wait_key = true;
    }

    /// Start a new timeline and return its init segment.
    fn setup(&mut self) -> Option<Vec<u8>> {
        self.time = 0;
        self.seq = 1;
        self.last_pts = None;
        self.cfg.as_ref().map(init_segment)
    }

    /// Push one scrcpy packet (`pts` in microseconds).
    ///
    /// Config packets without SPS/PPS are ignored; a config whose SPS cannot
    /// be parsed is an error (the muxer keeps its previous state).
    pub fn push(
        &mut self,
        is_config: bool,
        is_key: bool,
        pts: u64,
        data: &[u8],
    ) -> Result<MuxOutput, MediaError> {
        let mut out = MuxOutput::default();
        if is_config {
            let cfg = match parse_config(data) {
                Ok(cfg) => cfg,
                Err(MediaError::NoParameterSets) => return Ok(out),
                Err(e) => return Err(e),
            };
            let same = self.cfg.as_ref().is_some_and(|c| {
                c.codec == cfg.codec && c.width() == cfg.width() && c.height() == cfg.height()
            });
            self.cfg = Some(cfg);
            if !same {
                out.init_segment = self.setup();
            }
            self.wait_key = true;
            return Ok(out);
        }
        if self.cfg.is_none() && is_key {
            // Parameter sets sent in-band with the key frame.
            self.cfg = parse_config(data).ok();
            if self.cfg.is_some() {
                out.init_segment = self.setup();
            }
        }
        if self.cfg.is_none() {
            return Ok(out);
        }
        if self.wait_key && !is_key {
            self.dropped += 1;
            return Ok(out);
        }
        let sample = to_avcc_sample(data);
        if sample.is_empty() {
            return Ok(out);
        }
        let mut dt = match self.last_pts {
            None => 1.0 / 60.0,
            Some(last) => (pts as f64 - last as f64) / 1e6,
        };
        if dt.is_nan() || dt <= 0.0 {
            dt = 1.0 / 60.0;
        }
        dt = dt.clamp(1.0 / 240.0, 1.0 / 20.0);
        self.last_pts = Some(pts);
        let duration = (dt * f64::from(TIMESCALE)).round() as u32;
        out.media_segment = Some(fragment(self.seq, self.time, duration, &sample, is_key));
        self.seq = self.seq.wrapping_add(1);
        self.time += u64::from(duration);
        self.wait_key = false;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::test_util::{annex_b, make_sps};

    struct Mp4Box<'a> {
        kind: String,
        start: usize,
        size: usize,
        body: &'a [u8],
    }

    /// Parse sibling boxes in `buf[start..end]`; they must exactly fill it.
    fn boxes(buf: &[u8], start: usize, end: usize) -> Vec<Mp4Box<'_>> {
        let mut out = Vec::new();
        let mut o = start;
        while o < end {
            let size = u32::from_be_bytes(buf[o..o + 4].try_into().unwrap()) as usize;
            assert!(size >= 8, "bad box size at {o}");
            out.push(Mp4Box {
                kind: String::from_utf8_lossy(&buf[o + 4..o + 8]).into_owned(),
                start: o,
                size,
                body: &buf[o + 8..o + size],
            });
            o += size;
        }
        assert_eq!(o, end, "boxes must exactly fill their parent");
        out
    }

    fn find(hay: &[u8], needle: &[u8], from: usize) -> usize {
        from + hay[from..]
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("not found")
    }

    fn be16(b: &[u8], o: usize) -> u16 {
        u16::from_be_bytes([b[o], b[o + 1]])
    }

    fn be32(b: &[u8], o: usize) -> u32 {
        u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
    }

    const PPS: [u8; 4] = [0x68, 0xee, 0x3c, 0x80];

    #[test]
    fn avcc_box_record() {
        let sps = make_sps(1920, 1080, 100);
        let cfg = parse_config(&annex_b(&[&sps, &PPS])).unwrap();
        let avcc = avcc_box(&cfg);
        let b = boxes(&avcc, 0, avcc.len());
        assert_eq!(b[0].kind, "avcC");
        assert_eq!(&b[0].body[..6], &[1, 0x64, 0x00, 0x33, 0xff, 0xe1]);
        assert_eq!(be16(b[0].body, 6) as usize, sps.len());
        assert_eq!(b[0].body[8 + sps.len()], 1);
    }

    // Port of 'init segment is a well-formed ftyp + moov with the stream size'.
    #[test]
    fn init_segment_is_well_formed() {
        let cfg = parse_config(&annex_b(&[&make_sps(1832, 1920, 100), &PPS])).unwrap();
        let init = init_segment(&cfg);
        let top = boxes(&init, 0, init.len());
        assert_eq!(
            top.iter().map(|b| b.kind.as_str()).collect::<Vec<_>>(),
            ["ftyp", "moov"]
        );
        let moov = &top[1];
        let kids = boxes(&init, moov.start + 8, moov.start + moov.size);
        assert_eq!(
            kids.iter().map(|b| b.kind.as_str()).collect::<Vec<_>>(),
            ["mvhd", "trak", "mvex"]
        );
        for t in ["tkhd", "mdhd", "hdlr", "stsd", "avc1", "avcC", "trex"] {
            find(&init, t.as_bytes(), 0);
        }
        let avc1 = find(&init, b"avc1", find(&init, b"stsd", 0));
        // width/height live 24 bytes into the avc1 sample entry body
        assert_eq!(be16(&init, avc1 + 4 + 24), 1832);
        assert_eq!(be16(&init, avc1 + 4 + 26), 1920);
    }

    // Port of 'samples are AVCC length-prefixed without SPS/PPS/AUD'.
    #[test]
    fn samples_are_avcc_without_parameter_sets() {
        let sps = make_sps(640, 480, 100);
        let idr = [0x65, 0x88, 0x84, 0x00, 0x10];
        let sample = to_avcc_sample(&annex_b(&[&[0x09, 0xf0], &sps, &[0x68, 0xee], &idr]));
        assert_eq!(sample, [&[0, 0, 0, idr.len() as u8][..], &idr].concat());
        // 3-byte start codes too
        let s2 = to_avcc_sample(&[0, 0, 1, 0x41, 9, 8, 0, 0, 1, 0x41, 7]);
        assert_eq!(s2, [0, 0, 0, 3, 0x41, 9, 8, 0, 0, 0, 2, 0x41, 7]);
    }

    // Port of 'fragment: moof/mdat with correct data offset, tfdt and sample flags'.
    #[test]
    fn fragment_layout() {
        let sample = [0, 0, 0, 2, 0x65, 0xaa];
        for key in [true, false] {
            let frag = fragment(7, (1 << 33) + 5, 1500, &sample, key);
            let top = boxes(&frag, 0, frag.len());
            assert_eq!(
                top.iter().map(|b| b.kind.as_str()).collect::<Vec<_>>(),
                ["moof", "mdat"]
            );
            assert_eq!(top[1].body, sample);
            assert_eq!(be32(&frag, find(&frag, b"mfhd", 0) + 8), 7);
            let tfdt = find(&frag, b"tfdt", 0);
            assert_eq!(frag[tfdt + 4], 1); // version 1 (64-bit time)
            assert_eq!(
                u64::from_be_bytes(frag[tfdt + 8..tfdt + 16].try_into().unwrap()),
                (1 << 33) + 5
            );
            let trun = find(&frag, b"trun", 0);
            assert_eq!(be32(&frag, trun + 4) & 0xff_ffff, 0x000701);
            assert_eq!(be32(&frag, trun + 8), 1);
            assert_eq!(be32(&frag, trun + 12) as usize, top[1].start + 8); // data offset from moof start
            assert_eq!(be32(&frag, trun + 16), 1500);
            assert_eq!(be32(&frag, trun + 20) as usize, sample.len());
            assert_eq!(
                be32(&frag, trun + 24),
                if key { KEY_FLAGS } else { DELTA_FLAGS }
            );
        }
    }

    #[test]
    fn muxer_waits_for_key_frame_and_keeps_contiguous_timeline() {
        let config = annex_b(&[&make_sps(1280, 720, 100), &PPS]);
        let idr = annex_b(&[&[0x65, 1, 2, 3]]);
        let delta = annex_b(&[&[0x41, 4, 5]]);
        let mut m = Fmp4Muxer::new();
        // Nothing before the config.
        assert_eq!(
            m.push(false, false, 0, &delta).unwrap(),
            MuxOutput::default()
        );
        let out = m.push(true, false, 0, &config).unwrap();
        assert!(out.init_segment.is_some() && out.media_segment.is_none());
        assert_eq!(m.config().unwrap().codec, "avc1.640033");
        // Deltas before the first key frame are dropped.
        assert!(
            m.push(false, false, 10_000, &delta)
                .unwrap()
                .media_segment
                .is_none()
        );
        assert_eq!(m.dropped(), 1);
        let k = m
            .push(false, true, 1_000_000, &idr)
            .unwrap()
            .media_segment
            .unwrap();
        let d1 = m
            .push(false, false, 1_016_666, &delta)
            .unwrap()
            .media_segment
            .unwrap();
        // A long pause is clamped to 1/20 s; a pts going backwards counts as 1/60 s.
        let d2 = m
            .push(false, false, 3_000_000, &delta)
            .unwrap()
            .media_segment
            .unwrap();
        let d3 = m
            .push(false, false, 2_000_000, &delta)
            .unwrap()
            .media_segment
            .unwrap();
        let tfdt = |f: &[u8]| {
            let o = find(f, b"tfdt", 0);
            u64::from_be_bytes(f[o + 8..o + 16].try_into().unwrap())
        };
        let duration = |f: &[u8]| be32(f, find(f, b"trun", 0) + 16);
        assert_eq!(
            [tfdt(&k), tfdt(&d1), tfdt(&d2), tfdt(&d3)],
            [0, 1500, 3000, 7500]
        );
        assert_eq!(
            [duration(&k), duration(&d1), duration(&d2), duration(&d3)],
            [1500, 1500, 4500, 1500]
        );
        // Same parameters again: no new init segment, but wait for a key frame.
        let out = m.push(true, false, 0, &config).unwrap();
        assert!(out.init_segment.is_none());
        assert!(
            m.push(false, false, 2_100_000, &delta)
                .unwrap()
                .media_segment
                .is_none()
        );
        // New size: new init segment and a restarted timeline.
        let out = m
            .push(true, false, 0, &annex_b(&[&make_sps(720, 1280, 100), &PPS]))
            .unwrap();
        assert!(out.init_segment.is_some());
        let k2 = m
            .push(false, true, 5_000_000, &idr)
            .unwrap()
            .media_segment
            .unwrap();
        assert_eq!(tfdt(&k2), 0);
    }

    #[test]
    fn muxer_accepts_in_band_parameter_sets() {
        let mut m = Fmp4Muxer::new();
        let key = annex_b(&[&make_sps(640, 480, 66), &PPS, &[0x65, 9, 9]]);
        let out = m.push(false, true, 0, &key).unwrap();
        assert!(out.init_segment.is_some());
        let frag = out.media_segment.unwrap();
        let top = boxes(&frag, 0, frag.len());
        assert_eq!(top[1].body, [0, 0, 0, 3, 0x65, 9, 9]);
    }

    #[test]
    fn muxer_reports_bad_config_and_ignores_empty_config() {
        let mut m = Fmp4Muxer::new();
        assert_eq!(
            m.push(true, false, 0, &annex_b(&[&PPS])).unwrap(),
            MuxOutput::default()
        );
        assert!(
            m.push(true, false, 0, &annex_b(&[&[0x67, 0x64], &PPS]))
                .is_err()
        );
        assert!(m.config().is_none());
    }
}
