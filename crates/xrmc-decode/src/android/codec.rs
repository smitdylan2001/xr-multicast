//! `AMediaCodec` decoder (synchronous, short timeouts, ByteBuffer output).

use std::ffi::{CStr, CString, c_void};
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use ndk_sys as ffi;

use super::layout::{self, Crop, Layout};
use super::sps::{self, StreamInfo};
use super::{AndroidCodecInfo, candidates, codec_list, looks_software, mime};
use crate::stream::{Prepared, SpsColor, StreamState, default_bt709, h264_sps_color};
use crate::{
    Capabilities, Codec, CodecDetail, ColorInfo, DecodeCaps, DecodeError, Decoder, DecoderOptions,
    Frame,
};

const BUFFER_FLAG_CODEC_CONFIG: u32 = 2;
/// Picture size used when the stream's parameter sets can't be parsed.
const DEFAULT_SIZE: (u32, u32) = (1920, 1080);
/// How long to wait for a frame after queueing a packet (low-latency decoders answer in a few ms).
const OUTPUT_WAIT: Duration = Duration::from_millis(15);
/// After this many waits without a frame the decoder is treated as pipelined (no more waiting).
const PIPELINED_AFTER: u32 = 4;
/// Give up queueing a packet after this long (the decoder is stuck).
const INPUT_DEADLINE: Duration = Duration::from_millis(250);

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap_or_default()
}

/// An owned `AMediaFormat`.
struct Format(NonNull<ffi::AMediaFormat>);

impl Format {
    fn new() -> Option<Format> {
        NonNull::new(unsafe { ffi::AMediaFormat_new() }).map(Format)
    }

    fn set_i32(&mut self, key: &str, v: i32) {
        let k = cstr(key);
        unsafe { ffi::AMediaFormat_setInt32(self.0.as_ptr(), k.as_ptr(), v) }
    }

    fn set_str(&mut self, key: &str, v: &str) {
        let (k, v) = (cstr(key), cstr(v));
        unsafe { ffi::AMediaFormat_setString(self.0.as_ptr(), k.as_ptr(), v.as_ptr()) }
    }

    fn i32(&self, key: &str) -> Option<i32> {
        let k = cstr(key);
        let mut v = 0;
        unsafe { ffi::AMediaFormat_getInt32(self.0.as_ptr(), k.as_ptr(), &mut v) }.then_some(v)
    }

    fn buffer(&self, key: &str) -> Option<Vec<u8>> {
        let k = cstr(key);
        let mut data: *mut c_void = std::ptr::null_mut();
        let mut size = 0usize;
        let ok = unsafe {
            ffi::AMediaFormat_getBuffer(self.0.as_ptr(), k.as_ptr(), &mut data, &mut size)
        };
        if !ok || data.is_null() {
            return None;
        }
        Some(unsafe { std::slice::from_raw_parts(data as *const u8, size) }.to_vec())
    }

    fn rect(&self, key: &str) -> Option<(i32, i32, i32, i32)> {
        let k = cstr(key);
        let (mut l, mut t, mut r, mut b) = (0, 0, 0, 0);
        unsafe {
            ffi::AMediaFormat_getRect(self.0.as_ptr(), k.as_ptr(), &mut l, &mut t, &mut r, &mut b)
        }
        .then_some((l, t, r, b))
    }

    fn describe(&self) -> String {
        let p = unsafe { ffi::AMediaFormat_toString(self.0.as_ptr()) };
        if p.is_null() {
            return String::new();
        }
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

impl Drop for Format {
    fn drop(&mut self) {
        unsafe { ffi::AMediaFormat_delete(self.0.as_ptr()) };
    }
}

/// An owned `AMediaCodec`.
struct Mc {
    ptr: NonNull<ffi::AMediaCodec>,
    started: bool,
}

// SAFETY: an AMediaCodec may be used from any one thread at a time; `Mc` is only used by its owner.
unsafe impl Send for Mc {}

impl Mc {
    fn by_name(name: &str) -> Option<Mc> {
        let n = cstr(name);
        NonNull::new(unsafe { ffi::AMediaCodec_createCodecByName(n.as_ptr()) }).map(|ptr| Mc {
            ptr,
            started: false,
        })
    }

    fn by_type(mime: &str) -> Option<Mc> {
        let m = cstr(mime);
        NonNull::new(unsafe { ffi::AMediaCodec_createDecoderByType(m.as_ptr()) }).map(|ptr| Mc {
            ptr,
            started: false,
        })
    }

    fn p(&self) -> *mut ffi::AMediaCodec {
        self.ptr.as_ptr()
    }

    fn name(&self) -> Option<String> {
        let mut out: *mut std::os::raw::c_char = std::ptr::null_mut();
        let st = unsafe { ffi::AMediaCodec_getName(self.p(), &mut out) };
        if st != ffi::media_status_t::AMEDIA_OK || out.is_null() {
            return None;
        }
        let s = unsafe { CStr::from_ptr(out) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::AMediaCodec_releaseName(self.p(), out) };
        Some(s)
    }

    fn configure(&mut self, f: &Format) -> Result<(), String> {
        if self.started {
            unsafe { ffi::AMediaCodec_stop(self.p()) };
            self.started = false;
        }
        let st = unsafe {
            ffi::AMediaCodec_configure(
                self.p(),
                f.0.as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            )
        };
        if st != ffi::media_status_t::AMEDIA_OK {
            return Err(format!("configure failed ({})", st.0));
        }
        let st = unsafe { ffi::AMediaCodec_start(self.p()) };
        if st != ffi::media_status_t::AMEDIA_OK {
            return Err(format!("start failed ({})", st.0));
        }
        self.started = true;
        Ok(())
    }

    fn output_format(&self) -> Option<Format> {
        NonNull::new(unsafe { ffi::AMediaCodec_getOutputFormat(self.p()) }).map(Format)
    }
}

impl Drop for Mc {
    fn drop(&mut self) {
        unsafe {
            if self.started {
                ffi::AMediaCodec_stop(self.p());
            }
            ffi::AMediaCodec_delete(self.p());
        }
    }
}

/// How decoded buffers are laid out (from the output format).
#[derive(Debug, Clone, Copy)]
struct OutFormat {
    layout: Layout,
    crop: Crop,
    color: Option<ColorInfo>,
}

fn read_out_format(f: &Format) -> Option<OutFormat> {
    let w = f.i32("width")?.max(0) as u32;
    let h = f.i32("height")?.max(0) as u32;
    let layout = f
        .buffer("image-data")
        .and_then(|d| layout::from_media_image2(&d))
        .or_else(|| {
            layout::from_color_format(
                f.i32("color-format")?,
                w,
                h,
                f.i32("stride").unwrap_or(0).max(0) as u32,
                f.i32("slice-height").unwrap_or(0).max(0) as u32,
            )
        })?;
    let crop = match f.rect("crop") {
        Some((l, t, r, b)) if r >= l && b >= t => Crop {
            left: l.max(0) as u32,
            top: t.max(0) as u32,
            width: (r - l + 1) as u32,
            height: (b - t + 1) as u32,
        },
        _ => match (
            f.i32("crop-left"),
            f.i32("crop-top"),
            f.i32("crop-right"),
            f.i32("crop-bottom"),
        ) {
            (Some(l), Some(t), Some(r), Some(b)) if r >= l && b >= t => Crop {
                left: l.max(0) as u32,
                top: t.max(0) as u32,
                width: (r - l + 1) as u32,
                height: (b - t + 1) as u32,
            },
            _ => Crop {
                left: 0,
                top: 0,
                width: w.min(layout.width),
                height: h.min(layout.height),
            },
        },
    };
    let crop = Crop {
        width: crop.width.min(layout.width.saturating_sub(crop.left)),
        height: crop.height.min(layout.height.saturating_sub(crop.top)),
        ..crop
    };
    // MediaFormat COLOR_RANGE_FULL = 1, COLOR_STANDARD_BT709 = 1.
    let color = match (f.i32("color-range"), f.i32("color-standard")) {
        (None, None) => None,
        (range, standard) => Some(ColorInfo {
            full_range: range == Some(1),
            bt709: match standard {
                Some(s) => s == 1,
                None => default_bt709(crop.width, crop.height),
            },
        }),
    };
    Some(OutFormat {
        layout,
        crop,
        color,
    })
}

/// A MediaCodec decoder for one stream.
pub struct MediaCodecDecoder {
    codec: Codec,
    /// Decoders to try, best first (names); empty = by MIME type.
    names: Vec<(String, bool)>,
    mc: Option<Mc>,
    /// Index in `names` of the current decoder.
    current: usize,
    backend: String,
    hw: bool,
    configured: Option<StreamInfo>,
    out: Option<OutFormat>,
    stream: StreamState,
    sps_color: SpsColor,
    pending: u32,
    wait: Duration,
    missed: u32,
    logged_format: bool,
}

fn media_err(what: &str, code: isize) -> DecodeError {
    DecodeError::Decode(format!("mediacodec: {what} ({code})"))
}

impl MediaCodecDecoder {
    pub fn open(codec: Codec, _options: DecoderOptions) -> Result<MediaCodecDecoder, DecodeError> {
        let names: Vec<(String, bool)> = match codec_list() {
            Some(list) if !list.is_empty() => candidates(list, codec)
                .into_iter()
                .map(|i| (i.name.clone(), i.hardware && !i.software_only))
                .collect(),
            _ => Vec::new(),
        };
        if codec_list().is_some_and(|l| !l.is_empty()) && names.is_empty() {
            return Err(DecodeError::Unsupported(codec));
        }
        let mut d = MediaCodecDecoder {
            codec,
            names,
            mc: None,
            current: 0,
            backend: String::new(),
            hw: false,
            configured: None,
            out: None,
            stream: StreamState::new(codec),
            sps_color: SpsColor::default(),
            pending: 0,
            wait: OUTPUT_WAIT,
            missed: 0,
            logged_format: false,
        };
        d.create(0)?;
        Ok(d)
    }

    /// Create (unconfigured) the `index`-th candidate decoder.
    fn create(&mut self, index: usize) -> Result<(), DecodeError> {
        let (mc, hw) = if self.names.is_empty() {
            let mc = Mc::by_type(mime(self.codec)).ok_or(DecodeError::Unsupported(self.codec))?;
            let hw = mc.name().is_some_and(|n| !looks_software(&n));
            (mc, hw)
        } else {
            let mut found = None;
            for (i, (name, hw)) in self.names.iter().enumerate().skip(index) {
                if let Some(mc) = Mc::by_name(name) {
                    found = Some((mc, *hw));
                    self.current = i;
                    break;
                }
                tracing::warn!("mediacodec: could not create {name}");
            }
            found.ok_or(DecodeError::Unsupported(self.codec))?
        };
        let name = mc.name().unwrap_or_else(|| mime(self.codec).to_string());
        self.backend = format!("mediacodec:{name}");
        self.hw = hw;
        self.mc = Some(mc);
        self.configured = None;
        self.out = None;
        self.pending = 0;
        Ok(())
    }

    fn format_for(&self, info: StreamInfo, ten_bit_output: bool) -> Result<Format, DecodeError> {
        let mut f = Format::new().ok_or(DecodeError::Decode("AMediaFormat_new".into()))?;
        f.set_str("mime", mime(self.codec));
        f.set_i32("width", info.width as i32);
        f.set_i32("height", info.height as i32);
        f.set_i32(
            "color-format",
            if ten_bit_output {
                layout::COLOR_YUV_P010
            } else {
                layout::COLOR_YUV420_FLEXIBLE
            },
        );
        f.set_i32(
            "max-input-size",
            ((info.width * info.height * 3 / 2) as i32).max(2 << 20),
        );
        // No added latency: output each frame as soon as it is decoded.
        f.set_i32("low-latency", 1);
        f.set_i32("priority", 0);
        f.set_i32("vendor.qti-ext-dec-low-latency.enable", 1);
        f.set_i32("vendor.qti-ext-dec-picture-order.enable", 1);
        f.set_i32("vendor.rtc-ext-dec-low-latency.enable", 1);
        Ok(f)
    }

    /// Configure (or reconfigure) for a stream; tries 10-bit output, then any output, then the next decoder.
    fn configure(&mut self, info: StreamInfo) -> Result<(), DecodeError> {
        if self.mc.is_none() {
            self.create(self.current)?;
        }
        let tries: &[bool] = if info.bit_depth > 8 {
            &[true, false]
        } else {
            &[false]
        };
        let mut last = String::new();
        loop {
            for &ten in tries {
                let f = self.format_for(info, ten)?;
                let Some(mc) = self.mc.as_mut() else { break };
                match mc.configure(&f) {
                    Ok(()) => {
                        tracing::info!(
                            "{}: configured {}×{} {}-bit{}",
                            self.backend,
                            info.width,
                            info.height,
                            info.bit_depth,
                            if ten { " (P010 output)" } else { "" }
                        );
                        self.configured = Some(info);
                        self.out = None;
                        self.pending = 0;
                        self.logged_format = false;
                        return Ok(());
                    }
                    Err(e) => {
                        tracing::warn!("{}: {e} ({})", self.backend, f.describe());
                        last = e;
                        // A failed configure leaves the codec unusable: use a fresh instance.
                        self.mc = None;
                        self.create(self.current)?;
                    }
                }
            }
            // The next candidate decoder, if any.
            if self.current + 1 >= self.names.len() {
                break;
            }
            self.mc = None;
            self.create(self.current + 1)?;
        }
        self.mc = None;
        Err(DecodeError::Decode(format!("mediacodec: {last}")))
    }

    /// Dequeue every available output buffer; convert only the newest one.
    fn drain(&mut self, first_wait: Duration) -> Result<Option<Frame>, DecodeError> {
        let Some(mc) = self.mc.as_ref() else {
            return Ok(None);
        };
        let p = mc.p();
        let mut latest: Option<(usize, ffi::AMediaCodecBufferInfo)> = None;
        let mut timeout = first_wait;
        loop {
            let mut info = ffi::AMediaCodecBufferInfo {
                offset: 0,
                size: 0,
                presentationTimeUs: 0,
                flags: 0,
            };
            let r = unsafe {
                ffi::AMediaCodec_dequeueOutputBuffer(p, &mut info, timeout.as_micros() as i64)
            };
            timeout = Duration::ZERO;
            if r >= 0 {
                self.pending = self.pending.saturating_sub(1);
                if info.flags & BUFFER_FLAG_CODEC_CONFIG != 0 || info.size <= 0 {
                    unsafe { ffi::AMediaCodec_releaseOutputBuffer(p, r as usize, false) };
                    continue;
                }
                if let Some((old, _)) = latest.replace((r as usize, info)) {
                    unsafe { ffi::AMediaCodec_releaseOutputBuffer(p, old, false) };
                }
                continue;
            }
            match r as i32 {
                ffi::AMEDIACODEC_INFO_TRY_AGAIN_LATER => break,
                ffi::AMEDIACODEC_INFO_OUTPUT_FORMAT_CHANGED => {
                    if let Some(f) = mc.output_format() {
                        if !self.logged_format {
                            self.logged_format = true;
                            tracing::info!("{}: output {}", self.backend, f.describe());
                        }
                        self.out = read_out_format(&f);
                        if self.out.is_none() {
                            tracing::warn!(
                                "{}: unsupported output format {}",
                                self.backend,
                                f.describe()
                            );
                        }
                    }
                }
                ffi::AMEDIACODEC_INFO_OUTPUT_BUFFERS_CHANGED => {}
                _ => {
                    if let Some((old, _)) = latest {
                        unsafe { ffi::AMediaCodec_releaseOutputBuffer(p, old, false) };
                    }
                    return Err(media_err("dequeueOutputBuffer", r));
                }
            }
        }
        let Some((index, info)) = latest else {
            return Ok(None);
        };
        let frame = self.convert(index, &info);
        unsafe { ffi::AMediaCodec_releaseOutputBuffer(p, index, false) };
        Ok(frame)
    }

    fn convert(&mut self, index: usize, info: &ffi::AMediaCodecBufferInfo) -> Option<Frame> {
        let mc = self.mc.as_ref()?;
        if self.out.is_none() {
            // Some decoders never report a format change before the first buffer.
            self.out = mc.output_format().and_then(|f| read_out_format(&f));
        }
        let out = self.out?;
        let mut size = 0usize;
        let ptr = unsafe { ffi::AMediaCodec_getOutputBuffer(mc.p(), index, &mut size) };
        if ptr.is_null() {
            return None;
        }
        let all = unsafe { std::slice::from_raw_parts(ptr, size) };
        let start = (info.offset.max(0) as usize).min(all.len());
        let end = (start + info.size.max(0) as usize).min(all.len());
        let color = out.color.unwrap_or_else(|| {
            let bt709 = match (self.codec, self.sps_color.matrix) {
                (Codec::H264, Some(m)) => m == 1,
                _ => default_bt709(out.crop.width, out.crop.height),
            };
            ColorInfo {
                bt709,
                full_range: self.sps_color.full_range.unwrap_or(false),
            }
        });
        let frame = layout::to_frame(
            &all[start..end],
            &out.layout,
            out.crop,
            color,
            info.presentationTimeUs.max(0) as u64,
        );
        if frame.is_none() {
            tracing::warn!(
                "{}: buffer {} bytes does not match {:?}",
                self.backend,
                end - start,
                out
            );
        }
        frame
    }

    fn fail(&mut self, e: DecodeError) -> DecodeError {
        // Start again from the next key frame with a fresh decoder.
        self.stream.on_error();
        self.mc = None;
        self.configured = None;
        e
    }
}

impl Decoder for MediaCodecDecoder {
    fn push(
        &mut self,
        data: &[u8],
        pts: u64,
        key: bool,
        config: bool,
    ) -> Result<Vec<Frame>, DecodeError> {
        if config && self.codec == Codec::H264 {
            self.sps_color = h264_sps_color(data).unwrap_or_default();
        }
        let payload = match self.stream.prepare(data, key, config) {
            Prepared::Skip => return Ok(Vec::new()),
            Prepared::Feed(p) => p,
        };
        if key {
            let parsed = sps::parse(self.codec, &payload);
            let want = parsed.or(self.configured).unwrap_or(StreamInfo {
                width: DEFAULT_SIZE.0,
                height: DEFAULT_SIZE.1,
                bit_depth: 8,
            });
            let same = self.configured.is_some_and(|c| {
                (c.width, c.height, c.bit_depth) == (want.width, want.height, want.bit_depth)
            });
            if (self.mc.is_none() || !same)
                && let Err(e) = self.configure(want)
            {
                return Err(self.fail(e));
            }
        }
        let Some(p) = self.mc.as_ref().map(Mc::p) else {
            self.stream.on_error();
            return Err(DecodeError::Decode("mediacodec: not configured".into()));
        };
        // Queue the packet (draining output while the decoder is full).
        let started = Instant::now();
        let mut early: Option<Frame> = None;
        let index = loop {
            let r = unsafe { ffi::AMediaCodec_dequeueInputBuffer(p, 10_000) };
            if r >= 0 {
                break r as usize;
            }
            if r as i32 != ffi::AMEDIACODEC_INFO_TRY_AGAIN_LATER {
                let e = media_err("dequeueInputBuffer", r);
                return Err(self.fail(e));
            }
            match self.drain(Duration::ZERO) {
                Ok(Some(f)) => early = Some(f),
                Ok(None) => {}
                Err(e) => return Err(self.fail(e)),
            }
            if started.elapsed() > INPUT_DEADLINE {
                let e = DecodeError::Decode("mediacodec: the decoder accepts no input".into());
                return Err(self.fail(e));
            }
        };
        let mut cap = 0usize;
        let buf = unsafe { ffi::AMediaCodec_getInputBuffer(p, index, &mut cap) };
        if buf.is_null() || cap < payload.len() {
            unsafe { ffi::AMediaCodec_queueInputBuffer(p, index, 0, 0, pts, 0) };
            let e = DecodeError::Decode(format!(
                "mediacodec: packet of {} bytes does not fit the input buffer ({cap})",
                payload.len()
            ));
            return Err(self.fail(e));
        }
        unsafe { std::ptr::copy_nonoverlapping(payload.as_ptr(), buf, payload.len()) };
        let st = unsafe { ffi::AMediaCodec_queueInputBuffer(p, index, 0, payload.len(), pts, 0) };
        if st != ffi::media_status_t::AMEDIA_OK {
            let e = media_err("queueInputBuffer", st.0 as isize);
            return Err(self.fail(e));
        }
        self.stream.fed();
        self.pending += 1;
        let wait = if self.missed >= PIPELINED_AFTER {
            Duration::ZERO
        } else {
            self.wait
        };
        let frame = match self.drain(wait) {
            Ok(f) => f,
            Err(e) => return Err(self.fail(e)),
        };
        if wait > Duration::ZERO {
            if frame.is_some() {
                self.missed = 0;
            } else if self.pending > 0 {
                self.missed += 1;
                if self.missed == PIPELINED_AFTER {
                    tracing::info!(
                        "{}: decoder holds frames back; not waiting for output",
                        self.backend
                    );
                }
            }
        }
        Ok(frame.or(early).into_iter().collect())
    }

    fn backend(&self) -> &str {
        &self.backend
    }

    fn is_hardware(&self) -> bool {
        self.hw
    }

    fn waiting_for_key_frame(&self) -> bool {
        self.stream.waiting_for_key()
    }
}

/// Capabilities from the app's codec list (or by trying to create decoders).
pub fn probe_capabilities() -> Capabilities {
    const PROFILES: [(&str, Codec, bool); 5] = [
        ("h264", Codec::H264, false),
        ("h265", Codec::H265, false),
        ("av1", Codec::Av1, false),
        ("h265_10", Codec::H265, true),
        ("av1_10", Codec::Av1, true),
    ];
    let list: Vec<AndroidCodecInfo> = match codec_list() {
        Some(l) if !l.is_empty() => l.to_vec(),
        _ => {
            // No list from the app: try the default decoder per type.
            [Codec::H264, Codec::H265, Codec::Av1]
                .into_iter()
                .filter_map(|c| {
                    let name = Mc::by_type(mime(c))?.name()?;
                    Some(AndroidCodecInfo {
                        hardware: !looks_software(&name),
                        name,
                        mime: mime(c).into(),
                        ten_bit: c != Codec::H264,
                        ..Default::default()
                    })
                })
                .collect()
        }
    };
    let oh = crate::openh264::version();
    let mut details = Vec::new();
    let mut caps = |key: &str, codec: Codec, ten: bool| -> DecodeCaps {
        let best = candidates(&list, codec)
            .into_iter()
            .find(|i| !ten || i.ten_bit);
        let c = DecodeCaps {
            any: best.is_some() || (codec == Codec::H264 && oh.is_some()),
            hw: best.is_some_and(|i| i.hardware && !i.software_only),
        };
        details.push(CodecDetail {
            key: key.into(),
            caps: c,
            decoder: match best {
                Some(i) => i.name.clone(),
                None if c.any => "OpenH264".into(),
                None => String::new(),
            },
            hw_api: c.hw.then(|| "MediaCodec".into()),
        });
        c
    };
    let mut out = Capabilities::default();
    for (key, codec, ten) in PROFILES {
        let c = caps(key, codec, ten);
        match key {
            "h264" => out.h264 = c,
            "h265" => out.h265 = c,
            "av1" => out.av1 = c,
            "h265_10" => out.h265_10 = c,
            _ => out.av1_10 = c,
        }
    }
    // Keep the support-key order of the other backends.
    details.sort_by_key(|d| {
        ["h264", "h265", "av1", "h265_10", "av1_10"]
            .iter()
            .position(|k| *k == d.key)
    });
    let names: Vec<String> = details
        .iter()
        .filter(|d| d.caps.any && !d.key.ends_with("_10"))
        .map(|d| d.decoder.clone())
        .collect();
    out.backend = format!("MediaCodec [{}]", names.join(", "));
    if let Some(v) = oh {
        out.backend.push_str(&format!(" + OpenH264 {v}"));
    }
    out.details = details;
    out
}
