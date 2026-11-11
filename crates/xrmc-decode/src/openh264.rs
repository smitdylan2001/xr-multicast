//! OpenH264 backend: H.264 only, always available on Linux (built from source by
//! the `openh264` crate). Other targets build it from source with the
//! `openh264-source` feature, or load a Cisco OpenH264 binary at runtime
//! (`XRMC_OPENH264_LIB`, or `openh264*.dll` / `libopenh264*.dylib|.so` next to the
//! executable).

use std::sync::OnceLock;

use openh264::OpenH264API;
use openh264::decoder::{Decoder as OhDecoder, DecoderConfig, Flush};
use openh264::formats::YUVSource;

use crate::stream::{Prepared, SpsColor, StreamState, default_bt709, h264_sps_color};
use crate::{Codec, ColorInfo, DecodeError, Decoder, Frame, PixelFormat, Plane};

#[cfg(any(target_os = "linux", feature = "openh264-source"))]
fn api() -> Option<OpenH264API> {
    Some(OpenH264API::from_source())
}

#[cfg(not(any(target_os = "linux", feature = "openh264-source")))]
fn api() -> Option<OpenH264API> {
    let path = blob_path()?;
    match OpenH264API::from_blob_path(path) {
        Ok(api) => Some(api),
        Err(e) if std::env::var_os("XRMC_OPENH264_LIB").is_some() => {
            // An explicitly configured library that isn't one of Cisco's known builds.
            tracing::warn!(
                "OpenH264 {}: {e}; loading it anyway (XRMC_OPENH264_LIB)",
                path.display()
            );
            unsafe { OpenH264API::from_blob_path_unchecked(path) }.ok()
        }
        Err(e) => {
            tracing::warn!("OpenH264 {}: {e}", path.display());
            None
        }
    }
}

#[cfg(not(any(target_os = "linux", feature = "openh264-source")))]
fn blob_path() -> Option<&'static std::path::Path> {
    static PATH: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        if let Some(p) = std::env::var_os("XRMC_OPENH264_LIB").filter(|p| !p.is_empty()) {
            return Some(p.into());
        }
        let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
        let mut found: Vec<_> = std::fs::read_dir(&dir)
            .ok()?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                let name = p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                (name.starts_with("openh264") || name.starts_with("libopenh264"))
                    && (name.ends_with(".dll") || name.ends_with(".dylib") || name.contains(".so"))
            })
            .collect();
        found.sort();
        found.pop()
    })
    .as_deref()
}

/// Whether OpenH264 can be used, and its version (e.g. `2.6.0`).
pub fn version() -> Option<&'static str> {
    static VERSION: OnceLock<Option<String>> = OnceLock::new();
    VERSION
        .get_or_init(|| {
            use openh264_sys2::API;
            let api = api()?;
            // SAFETY: plain query of a static version struct.
            let v = unsafe { api.WelsGetCodecVersion() };
            Some(format!("{}.{}.{}", v.uMajor, v.uMinor, v.uRevision))
        })
        .as_deref()
}

pub struct OpenH264Decoder {
    dec: OhDecoder,
    stream: StreamState,
    sps_color: SpsColor,
}

fn new_decoder() -> Result<OhDecoder, DecodeError> {
    let api = api().ok_or(DecodeError::Unsupported(Codec::H264))?;
    // Flush after a decode that returned no picture: OpenH264 holds back the first
    // picture of non-Baseline streams for reordering; scrcpy streams have no B-frames.
    let config = DecoderConfig::new().flush_after_decode(Flush::Flush);
    OhDecoder::with_api_config(api, config)
        .map_err(|e| DecodeError::Decode(format!("openh264: {e}")))
}

impl OpenH264Decoder {
    pub fn open() -> Result<OpenH264Decoder, DecodeError> {
        Ok(OpenH264Decoder {
            dec: new_decoder()?,
            stream: StreamState::new(Codec::H264),
            sps_color: SpsColor::default(),
        })
    }
}

impl Decoder for OpenH264Decoder {
    fn push(
        &mut self,
        data: &[u8],
        pts: u64,
        key: bool,
        config: bool,
    ) -> Result<Vec<Frame>, DecodeError> {
        if config {
            self.sps_color = h264_sps_color(data).unwrap_or_default();
        }
        let payload = match self.stream.prepare(data, key, config) {
            Prepared::Skip => return Ok(Vec::new()),
            Prepared::Feed(p) => p,
        };
        if key
            && self.stream.prefix().is_none()
            && let Some(c) = h264_sps_color(&payload)
        {
            // Parameter sets in-band only.
            self.sps_color = c;
        }
        let sps_color = self.sps_color;
        let yuv = match self.dec.decode(&payload) {
            Ok(yuv) => yuv,
            Err(e) => {
                self.stream.on_error();
                let err = DecodeError::Decode(format!("openh264: {e}"));
                // After a bitstream error OpenH264 can reject the next key frame
                // (it resets lazily and drops the parameter sets we prepend), so
                // start over with a fresh decoder.
                match new_decoder() {
                    Ok(dec) => self.dec = dec,
                    Err(e2) => tracing::warn!("openh264: recreate after error failed: {e2}"),
                }
                return Err(err);
            }
        };
        self.stream.fed();
        let Some(yuv) = yuv else {
            return Ok(Vec::new());
        };
        let (w, h) = yuv.dimensions();
        let (sy, su, sv) = yuv.strides();
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let copy = |src: &[u8], stride: usize, pw: usize, ph: usize| -> Option<Plane> {
            let mut data = Vec::with_capacity(pw * ph);
            for y in 0..ph {
                data.extend_from_slice(src.get(y * stride..y * stride + pw)?);
            }
            Some(Plane {
                data,
                stride: pw,
                width: pw as u32,
                height: ph as u32,
            })
        };
        let planes = [
            copy(yuv.y(), sy, w, h),
            copy(yuv.u(), su, cw, ch),
            copy(yuv.v(), sv, cw, ch),
        ];
        let planes: Option<Vec<Plane>> = planes.into_iter().collect();
        let planes = planes.ok_or_else(|| DecodeError::Decode("openh264: short plane".into()))?;
        let (w, h) = (w as u32, h as u32);
        let bt709 = match sps_color.matrix {
            Some(1 | 7 | 9 | 10) => true,
            Some(4..=6) => false,
            _ => default_bt709(w, h),
        };
        Ok(vec![Frame {
            width: w,
            height: h,
            format: PixelFormat::I420,
            color: ColorInfo {
                bt709,
                full_range: sps_color.full_range.unwrap_or(false),
            },
            planes,
            pts,
        }])
    }

    fn backend(&self) -> &str {
        "openh264"
    }

    fn waiting_for_key_frame(&self) -> bool {
        self.stream.waiting_for_key()
    }
}
