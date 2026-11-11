//! Pure (no I/O) media helpers for XR MultiCast.
//!
//! * Annex B / NAL unit helpers and an Exp-Golomb [`bits::BitReader`].
//! * Codec strings (RFC 6381, as used by WebCodecs and MSE), bit depths and
//!   frame sizes from H.264/H.265 SPS and AV1 `av1C`/sequence headers.
//! * [`describe_config`]: what a decoder needs to know about a scrcpy config
//!   packet (port of `describeConfig` in the Electron app's `decoder.js`).
//! * [`fmp4`]: H.264 -> fragmented MP4 remuxer (port of `mse-decoder.js`).
//!
//! Nothing here panics on malformed input: parsers return [`MediaError`] or
//! fall back to safe defaults, like the JavaScript reference.

pub mod av1;
pub mod bits;
pub mod fmp4;
pub mod h264;
pub mod h265;
pub mod nal;

pub use fmp4::{Fmp4Muxer, MuxOutput};

/// Errors from the media parsers.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MediaError {
    /// The data ended before the structure being parsed.
    #[error("data truncated")]
    Truncated,
    /// A value is out of range or inconsistent.
    #[error("invalid data: {0}")]
    Invalid(&'static str),
    /// An H.264 buffer has no SPS and/or no PPS.
    #[error("no SPS/PPS in the config packet")]
    NoParameterSets,
}

/// The video codecs scrcpy-server can be asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoCodec {
    /// H.264 / AVC (Annex B stream).
    H264,
    /// H.265 / HEVC (Annex B stream).
    H265,
    /// AV1 (low-overhead OBU stream).
    Av1,
}

impl VideoCodec {
    /// All codecs, in the app's automatic preference order (most efficient first).
    pub const ALL: [VideoCodec; 3] = [VideoCodec::H265, VideoCodec::Av1, VideoCodec::H264];

    /// Short name as used by scrcpy and the app: `h264`, `h265`, `av1`.
    pub fn name(self) -> &'static str {
        match self {
            VideoCodec::H264 => "h264",
            VideoCodec::H265 => "h265",
            VideoCodec::Av1 => "av1",
        }
    }

    /// Parse a short name (`h264`, `h265`, `av1`).
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "h264" => Some(VideoCodec::H264),
            "h265" => Some(VideoCodec::H265),
            "av1" => Some(VideoCodec::Av1),
            _ => None,
        }
    }

    /// Codec string assumed when the stream gives no better information
    /// (also what the app probes decoders with).
    pub fn default_codec_string(self) -> &'static str {
        match self {
            VideoCodec::H264 => h264::DEFAULT_CODEC,
            VideoCodec::H265 => h265::DEFAULT_CODEC,
            VideoCodec::Av1 => av1::DEFAULT_CODEC,
        }
    }

    /// Codec string to probe 10-bit decoding with, for codecs where the app
    /// supports 10-bit streams.
    pub fn ten_bit_codec_string(self) -> Option<&'static str> {
        match self {
            VideoCodec::H264 => None,
            VideoCodec::H265 => Some(h265::MAIN10_CODEC),
            VideoCodec::Av1 => Some(av1::MAIN10_CODEC),
        }
    }
}

impl std::fmt::Display for VideoCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// What a decoder needs to know about a stream, from its config packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoConfig {
    /// RFC 6381 codec string, e.g. `avc1.640033`, `hvc1.2.4.L123.90`, `av01.0.08M.10`.
    pub codec: String,
    /// Out-of-band decoder description: the `av1C` record for AV1 when the
    /// config packet is one. `None` for Annex B streams (H.264/H.265) and raw
    /// AV1 OBUs, whose parameters travel in-band.
    pub description: Option<Vec<u8>>,
    /// Whether the config packet must be prepended to every key frame so a
    /// decoder can (re)start from it (true unless there is a `description`).
    pub prepend_to_key_frames: bool,
    /// Luma bit depth (8, 10 or 12).
    pub bit_depth: u8,
    /// Frame size in pixels (width, height) when the config reveals it.
    /// For AV1 this is the maximum frame size from the sequence header.
    pub dimensions: Option<(u32, u32)>,
}

/// Describe a scrcpy config packet (port of `describeConfig` in `decoder.js`).
///
/// * H.264: `avc1.PPCCLL` from the SPS (default `avc1.640033`), prepend.
///   The bit depth comes from the SPS when it parses (the JS always says 8).
/// * H.265: RFC 6381 string with Main -> Main 10 correction for mislabelled
///   10-bit streams, luma bit depth, prepend.
/// * AV1: an `av1C` record (first byte `0x81`) becomes the decoder
///   description (no prepend); raw OBUs are prepended, with the codec string
///   read from their sequence header when there is one (the JS uses the
///   default `av01.0.13M.08` there).
///
/// Never fails: anything unparseable falls back to the codec's default string
/// and 8-bit.
pub fn describe_config(codec: VideoCodec, config: &[u8]) -> VideoConfig {
    match codec {
        VideoCodec::H264 => {
            let sps = nal::split_nals(config)
                .into_iter()
                .find(|n| n.first().is_some_and(|h| h & 0x1f == h264::nal_type::SPS))
                .and_then(|n| h264::parse_sps(n).ok());
            VideoConfig {
                codec: h264::codec_string(config),
                description: None,
                prepend_to_key_frames: true,
                bit_depth: sps.map_or(8, |s| s.bit_depth_luma),
                dimensions: sps.map(|s| (s.width, s.height)),
            }
        }
        VideoCodec::H265 => {
            let info = h265::info(config);
            VideoConfig {
                codec: info.codec,
                description: None,
                prepend_to_key_frames: true,
                bit_depth: info.bit_depth,
                dimensions: info.dimensions,
            }
        }
        VideoCodec::Av1 => {
            if let Ok(params) = av1::av1c_params(config) {
                let seq = av1::find_sequence_header(&config[4..]).ok();
                VideoConfig {
                    codec: params.codec_string(),
                    description: Some(config.to_vec()),
                    prepend_to_key_frames: false,
                    bit_depth: params.bit_depth,
                    dimensions: seq.map(|s| (s.max_width, s.max_height)),
                }
            } else if let Ok(seq) = av1::find_sequence_header(config) {
                VideoConfig {
                    codec: seq.params.codec_string(),
                    description: None,
                    prepend_to_key_frames: true,
                    bit_depth: seq.params.bit_depth,
                    dimensions: Some((seq.max_width, seq.max_height)),
                }
            } else {
                VideoConfig {
                    codec: av1::DEFAULT_CODEC.to_owned(),
                    description: None,
                    prepend_to_key_frames: true,
                    bit_depth: 8,
                    dimensions: None,
                }
            }
        }
    }
}

/// Real SPS NAL units captured from encoders (from `test/decoder-config.test.js`).
#[cfg(test)]
pub(crate) mod test_vectors {
    pub const SPS_MAIN10_X265: &str = "00000142010102200000030090000003000003007ba00398803c1cbb65ba4a4c2fff000100016a02020208000003000800000301e04000";
    pub const SPS_10BIT_MISLABELLED_VAAPI: &str = "00000142010121600000030090000003000003007ba003a0803c1c6eda2e490a569ffc0004000404000003000400000300f02000";
    pub const SPS_MAIN8_VAAPI: &str = "00000142010121600000030090000003000003007ba003a0803c1c6fa2e490a569ffc00040004040000003004000000f0200";

    pub fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::test_vectors::*;
    use super::*;

    // Ports of test/decoder-config.test.js.

    #[test]
    fn hevc_main10_stream_gives_main10_codec_string_and_10_bit_depth() {
        let d = describe_config(VideoCodec::H265, &hex(SPS_MAIN10_X265));
        assert_eq!(d.codec, "hvc1.2.4.L123.90");
        assert_eq!(d.bit_depth, 10);
        assert!(d.prepend_to_key_frames);
        assert_eq!(d.description, None);
    }

    #[test]
    fn ten_bit_hevc_labelled_as_main_is_corrected_to_main10() {
        let d = describe_config(VideoCodec::H265, &hex(SPS_10BIT_MISLABELLED_VAAPI));
        assert_eq!(d.bit_depth, 10);
        assert!(d.codec.starts_with("hvc1.2.4.H123"), "{}", d.codec);
    }

    #[test]
    fn eight_bit_hevc_main_stays_main() {
        let d = describe_config(VideoCodec::H265, &hex(SPS_MAIN8_VAAPI));
        assert_eq!(d.bit_depth, 8);
        assert!(d.codec.starts_with("hvc1.1.6.H123"), "{}", d.codec);
    }

    #[test]
    fn av1_av1c_record_gives_codec_string_description_and_bit_depth() {
        let rec = hex("81084c000a0b0000");
        let d = describe_config(VideoCodec::Av1, &rec);
        assert_eq!(d.codec, "av01.0.08M.10");
        assert_eq!(d.bit_depth, 10);
        assert_eq!(d.description.as_deref(), Some(&rec[..]));
        assert!(!d.prepend_to_key_frames);
    }

    #[test]
    fn h264_codec_string_from_sps() {
        let d = describe_config(VideoCodec::H264, &hex("0000000167640033ac"));
        assert_eq!(d.codec, "avc1.640033");
        assert_eq!(d.bit_depth, 8);
        assert!(d.prepend_to_key_frames);
    }

    // Beyond the JS tests.

    #[test]
    fn h264_dimensions_from_sps() {
        let sps = h264::test_util::make_sps(3664, 1920, 100);
        let d = describe_config(
            VideoCodec::H264,
            &h264::test_util::annex_b(&[&sps, &[0x68, 0xee]]),
        );
        assert_eq!(d.dimensions, Some((3664, 1920)));
        assert_eq!(d.codec, "avc1.640033");
    }

    #[test]
    fn av1_raw_obus_use_the_sequence_header() {
        let obu = av1::test_util::sequence_header_obu(0, 12, false, true, 1920, 1080);
        let d = describe_config(VideoCodec::Av1, &obu);
        assert_eq!(d.codec, "av01.0.12M.10");
        assert_eq!((d.bit_depth, d.dimensions), (10, Some((1920, 1080))));
        assert!(d.prepend_to_key_frames && d.description.is_none());
        // av1C with its config OBUs also reveals the size.
        let mut rec = vec![0x81, 0x0c, 0x4c, 0x00];
        rec.extend_from_slice(&obu);
        assert_eq!(
            describe_config(VideoCodec::Av1, &rec).dimensions,
            Some((1920, 1080))
        );
        // Nothing usable: JS defaults.
        let d = describe_config(VideoCodec::Av1, &[0x12, 0x00]);
        assert_eq!(
            (d.codec.as_str(), d.bit_depth, d.prepend_to_key_frames),
            (av1::DEFAULT_CODEC, 8, true)
        );
    }

    #[test]
    fn defaults_for_empty_configs() {
        for c in VideoCodec::ALL {
            let d = describe_config(c, &[]);
            assert_eq!(d.codec, c.default_codec_string());
            assert_eq!(d.bit_depth, 8);
        }
    }

    #[test]
    fn codec_names_round_trip() {
        for c in VideoCodec::ALL {
            assert_eq!(VideoCodec::from_name(c.name()), Some(c));
        }
        assert_eq!(VideoCodec::from_name("vp9"), None);
    }

    #[test]
    fn random_configs_never_panic() {
        let mut seed = 0x2545_f491_u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for _ in 0..2000 {
            let len = (next() % 128) as usize;
            let mut data: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            // Bias towards plausible structure.
            match next() % 4 {
                0 => data.splice(0..0, [0, 0, 0, 1, 0x67]).for_each(drop),
                1 => data.splice(0..0, [0, 0, 1, 0x42, 0x01]).for_each(drop),
                2 => data.splice(0..0, [0x81]).for_each(drop),
                _ => data.splice(0..0, [0x0a]).for_each(drop),
            }
            for c in VideoCodec::ALL {
                let d = describe_config(c, &data);
                assert!(matches!(d.bit_depth, 8 | 10 | 12) || c != VideoCodec::Av1);
            }
        }
    }
}
