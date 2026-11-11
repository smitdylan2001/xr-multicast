//! scrcpy-server launch options and the shell command that starts it.

use std::borrow::Cow;
use std::fmt;
use std::hash::{BuildHasher, RandomState};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{REMOTE_SERVER_PATH, SCRCPY_VERSION, SERVER_CLASS};

/// Video codecs the app asks scrcpy-server for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum VideoCodec {
    /// H.264 / AVC (the universal baseline).
    #[default]
    H264,
    /// H.265 / HEVC.
    H265,
    /// AV1.
    Av1,
}

impl VideoCodec {
    /// Name as used in scrcpy arguments (`video_codec=`) and the app: `h264`, `h265`, `av1`.
    pub fn name(self) -> &'static str {
        match self {
            VideoCodec::H264 => "h264",
            VideoCodec::H265 => "h265",
            VideoCodec::Av1 => "av1",
        }
    }

    /// Parse a codec name (`h264`, `h265`, `av1`).
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "h264" => Some(VideoCodec::H264),
            "h265" => Some(VideoCodec::H265),
            "av1" => Some(VideoCodec::Av1),
            _ => None,
        }
    }

    /// The codec id scrcpy sends at the start of the video stream (its name as a big-endian u32).
    pub fn id(self) -> u32 {
        match self {
            VideoCodec::H264 => 0x6832_3634,
            VideoCodec::H265 => 0x6832_3635,
            VideoCodec::Av1 => 0x0061_7631,
        }
    }

    /// The codec for a stream codec id, if it is one of ours.
    pub fn from_id(id: u32) -> Option<Self> {
        [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1]
            .into_iter()
            .find(|c| c.id() == id)
    }
}

impl fmt::Display for VideoCodec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Human-readable name of a scrcpy stream codec id: `h264`, `h265`, `av1`,
/// `vp8`, `vp9`, or `0x…` for anything else.
pub fn codec_id_name(id: u32) -> String {
    match id {
        0x0076_7038 => "vp8".to_owned(),
        0x0076_7039 => "vp9".to_owned(),
        _ => VideoCodec::from_id(id).map_or_else(|| format!("0x{id:x}"), |c| c.name().to_owned()),
    }
}

/// The scrcpy connection id: a 31-bit number that names the device socket
/// (`scrcpy_<scid>`), so several clients can start at the same time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Scid(u32);

impl Scid {
    /// A scid from a raw value (the top bit is cleared to keep it 31-bit).
    pub fn new(value: u32) -> Self {
        Scid(value & 0x7fff_ffff)
    }

    /// A random scid.
    pub fn random() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        // RandomState is seeded from the OS RNG; hashing a counter and the time
        // gives a fresh value on every call without an extra dependency.
        let v = RandomState::new().hash_one((
            COUNTER.fetch_add(1, Ordering::Relaxed),
            nanos,
            std::process::id(),
        ));
        Scid::new((v ^ (v >> 32)) as u32)
    }

    /// The raw 31-bit value.
    pub fn value(self) -> u32 {
        self.0
    }

    /// The abstract socket the server listens on: `scrcpy_<scid>`.
    pub fn socket_name(self) -> String {
        format!("scrcpy_{self}")
    }

    /// The ADB service to open for the video stream: `localabstract:scrcpy_<scid>`.
    pub fn service(self) -> String {
        format!("localabstract:{}", self.socket_name())
    }
}

impl fmt::Display for Scid {
    /// `%08x`, as scrcpy expects.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:08x}", self.0)
    }
}

/// scrcpy-server log level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogLevel {
    /// Everything.
    Verbose,
    /// Debug and above.
    Debug,
    /// Info and above (the app's default: errors are read from the log).
    #[default]
    Info,
    /// Warnings and errors.
    Warn,
    /// Errors only.
    Error,
}

impl LogLevel {
    /// The argument value.
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Verbose => "verbose",
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }
}

/// A crop rectangle in device pixels (before scaling): `width:height:x:y`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Crop {
    /// Width of the cropped area.
    pub width: u32,
    /// Height of the cropped area.
    pub height: u32,
    /// Left offset.
    pub x: u32,
    /// Top offset.
    pub y: u32,
}

impl fmt::Display for Crop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}:{}", self.width, self.height, self.x, self.y)
    }
}

/// Capture orientation (clockwise rotation, optionally mirrored). Experimental in the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    /// `0`
    Rotate0,
    /// `90`
    Rotate90,
    /// `180`
    Rotate180,
    /// `270`
    Rotate270,
    /// `flip0`
    Flip0,
    /// `flip90`
    Flip90,
    /// `flip180`
    Flip180,
    /// `flip270`
    Flip270,
}

impl Orientation {
    /// The scrcpy name: `0|90|180|270|flip0|flip90|flip180|flip270`.
    pub fn as_str(self) -> &'static str {
        match self {
            Orientation::Rotate0 => "0",
            Orientation::Rotate90 => "90",
            Orientation::Rotate180 => "180",
            Orientation::Rotate270 => "270",
            Orientation::Flip0 => "flip0",
            Orientation::Flip90 => "flip90",
            Orientation::Flip180 => "flip180",
            Orientation::Flip270 => "flip270",
        }
    }

    /// Parse a scrcpy orientation name.
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "0" => Orientation::Rotate0,
            "90" => Orientation::Rotate90,
            "180" => Orientation::Rotate180,
            "270" => Orientation::Rotate270,
            "flip0" => Orientation::Flip0,
            "flip90" => Orientation::Flip90,
            "flip180" => Orientation::Flip180,
            "flip270" => Orientation::Flip270,
            _ => return None,
        })
    }
}

/// Everything that goes on the scrcpy-server command line for one stream.
///
/// The protocol options the session relies on (video only, forward tunnel,
/// dummy byte, device/frame/stream meta) are always set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerOptions {
    /// Connection id (random by default).
    pub scid: Scid,
    /// Server log level.
    pub log_level: LogLevel,
    /// Video codec.
    pub codec: VideoCodec,
    /// Maximum long side of the encoded video in pixels; 0 = native size.
    pub max_size: u32,
    /// Video bit rate in bits per second.
    pub video_bit_rate: u32,
    /// Maximum frame rate; 0 = unlimited.
    pub max_fps: u32,
    /// Seconds between key frames (frequent key frames let a decoder recover
    /// quickly after packet loss).
    pub i_frame_interval: u32,
    /// Ask for 10-bit (`profile=2`: HEVC Main 10 / AV1 Main 10-bit). Ignored for H.264.
    pub ten_bit: bool,
    /// Extra MediaFormat options for `video_codec_options` (`key[:type]=value`).
    pub codec_options: Vec<String>,
    /// Capture only this part of the display (e.g. one eye of a stereo panel).
    pub crop: Option<Crop>,
    /// Use this encoder (e.g. `c2.qti.hevc.encoder`) instead of the default one.
    pub video_encoder: Option<String>,
    /// Capture this display instead of the default one.
    pub display_id: Option<u32>,
    /// Capture orientation (experimental).
    pub capture_orientation: Option<Orientation>,
    /// Lock the capture orientation (`@` prefix) instead of following the device.
    pub lock_capture_orientation: bool,
    /// Whether the server turns the screen on at start (`power_on`); server default when `None`.
    pub power_on: Option<bool>,
    /// Raw `key=value` arguments appended last (so they override earlier keys).
    pub extra_args: Vec<String>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            scid: Scid::random(),
            log_level: LogLevel::Info,
            codec: VideoCodec::H264,
            max_size: 0,
            video_bit_rate: 8_000_000,
            max_fps: 60,
            i_frame_interval: 2,
            ten_bit: false,
            codec_options: Vec::new(),
            crop: None,
            video_encoder: None,
            display_id: None,
            capture_orientation: None,
            lock_capture_orientation: false,
            power_on: None,
            extra_args: Vec::new(),
        }
    }
}

impl ServerOptions {
    /// Default options (random scid, H.264, native size, 8 Mbps, 60 fps).
    pub fn new() -> Self {
        Self::default()
    }

    /// The `key=value` arguments after the version (unquoted).
    pub fn to_args(&self) -> Vec<String> {
        let mut codec_options = vec![format!("i-frame-interval={}", self.i_frame_interval)];
        // MediaCodecInfo.CodecProfileLevel: HEVCProfileMain10 = 2, AV1ProfileMain10 = 2
        if self.ten_bit && self.codec != VideoCodec::H264 {
            codec_options.push("profile=2".to_owned());
        }
        codec_options.extend(self.codec_options.iter().cloned());
        let mut args = vec![
            format!("scid={}", self.scid),
            format!("log_level={}", self.log_level.as_str()),
            "video=true".to_owned(),
            "audio=false".to_owned(),
            "control=false".to_owned(),
            "tunnel_forward=true".to_owned(),
            "cleanup=true".to_owned(),
            format!("video_codec={}", self.codec),
            format!("max_size={}", self.max_size),
            format!("video_bit_rate={}", self.video_bit_rate),
            format!("max_fps={}", self.max_fps),
            format!("video_codec_options={}", codec_options.join(",")),
            "send_device_meta=true".to_owned(),
            "send_frame_meta=true".to_owned(),
            "send_dummy_byte=true".to_owned(),
            "send_stream_meta=true".to_owned(),
        ];
        if let Some(crop) = self.crop {
            args.push(format!("crop={crop}"));
        }
        if let Some(encoder) = &self.video_encoder {
            args.push(format!("video_encoder={encoder}"));
        }
        if let Some(id) = self.display_id {
            args.push(format!("display_id={id}"));
        }
        if let Some(o) = self.capture_orientation {
            let lock = if self.lock_capture_orientation {
                "@"
            } else {
                ""
            };
            args.push(format!("capture_orientation={lock}{}", o.as_str()));
        }
        if let Some(on) = self.power_on {
            args.push(format!("power_on={on}"));
        }
        args.extend(self.extra_args.iter().cloned());
        args
    }

    /// The device shell command that starts the server from [`REMOTE_SERVER_PATH`].
    pub fn command(&self) -> String {
        server_command(REMOTE_SERVER_PATH, &self.to_args())
    }
}

/// `CLASSPATH=<remote_jar> app_process / com.genymobile.scrcpy.Server <version> <args>`,
/// with arguments shell-quoted where needed.
pub fn server_command<S: AsRef<str>>(remote_jar: &str, args: &[S]) -> String {
    let mut cmd = format!(
        "CLASSPATH={} app_process / {SERVER_CLASS} {SCRCPY_VERSION}",
        shell_quote(remote_jar)
    );
    for a in args {
        cmd.push(' ');
        cmd.push_str(&shell_quote(a.as_ref()));
    }
    cmd
}

/// Quote `s` for the device shell when it contains anything beyond a safe
/// set of characters (letters, digits and `_-.,:=@/+%`).
pub fn shell_quote(s: &str) -> Cow<'_, str> {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_-.,:=@/+%".contains(c);
    if !s.is_empty() && s.chars().all(safe) {
        Cow::Borrowed(s)
    } else {
        Cow::Owned(format!("'{}'", s.replace('\'', r"'\''")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_args_match_the_electron_app() {
        let o = ServerOptions {
            scid: Scid::new(0x0a1b2c3d),
            max_size: 1920,
            video_bit_rate: 8_000_000,
            max_fps: 60,
            ..ServerOptions::default()
        };
        assert_eq!(
            o.to_args(),
            [
                "scid=0a1b2c3d",
                "log_level=info",
                "video=true",
                "audio=false",
                "control=false",
                "tunnel_forward=true",
                "cleanup=true",
                "video_codec=h264",
                "max_size=1920",
                "video_bit_rate=8000000",
                "max_fps=60",
                "video_codec_options=i-frame-interval=2",
                "send_device_meta=true",
                "send_frame_meta=true",
                "send_dummy_byte=true",
                "send_stream_meta=true",
            ]
        );
        assert_eq!(
            o.command(),
            format!(
                "CLASSPATH=/data/local/tmp/xrmc-scrcpy-server.jar app_process / com.genymobile.scrcpy.Server 5.0.1 {}",
                o.to_args().join(" ")
            )
        );
    }

    #[test]
    fn optional_args() {
        let o = ServerOptions {
            scid: Scid::new(0xffff_ffff),
            codec: VideoCodec::H265,
            ten_bit: true,
            codec_options: vec!["latency:int=1".into()],
            crop: Some(Crop {
                width: 1832,
                height: 1920,
                x: 1832,
                y: 0,
            }),
            video_encoder: Some("c2.qti.hevc.encoder".into()),
            display_id: Some(3),
            capture_orientation: Some(Orientation::Flip90),
            lock_capture_orientation: true,
            power_on: Some(false),
            extra_args: vec!["min_size_alignment=16".into()],
            ..ServerOptions::default()
        };
        let args = o.to_args();
        assert_eq!(args[0], "scid=7fffffff");
        assert!(args.contains(&"video_codec=h265".to_owned()));
        assert!(args.contains(
            &"video_codec_options=i-frame-interval=2,profile=2,latency:int=1".to_owned()
        ));
        assert_eq!(
            &args[16..],
            [
                "crop=1832:1920:1832:0",
                "video_encoder=c2.qti.hevc.encoder",
                "display_id=3",
                "capture_orientation=@flip90",
                "power_on=false",
                "min_size_alignment=16",
            ]
        );
        // 10-bit only applies to HEVC/AV1.
        let h264 = ServerOptions {
            ten_bit: true,
            ..ServerOptions::default()
        };
        assert!(
            h264.to_args()
                .contains(&"video_codec_options=i-frame-interval=2".to_owned())
        );
    }

    #[test]
    fn scid_is_random_31_bit_hex() {
        let a = Scid::random();
        let b = Scid::random();
        assert_ne!(a, b);
        for s in [a, b, Scid::new(u32::MAX), Scid::new(5)] {
            assert!(s.value() <= 0x7fff_ffff);
            assert_eq!(s.to_string().len(), 8);
        }
        assert_eq!(Scid::new(5).service(), "localabstract:scrcpy_00000005");
        assert_ne!(ServerOptions::new().scid, ServerOptions::new().scid);
    }

    #[test]
    fn unsafe_arguments_are_quoted() {
        assert_eq!(
            shell_quote("video_encoder=c2.qti.avc.encoder"),
            "video_encoder=c2.qti.avc.encoder"
        );
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("x;reboot"), "'x;reboot'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
        let o = ServerOptions {
            video_encoder: Some("bad name; reboot".into()),
            ..ServerOptions::default()
        };
        assert!(o.command().ends_with(" 'video_encoder=bad name; reboot'"));
    }

    #[test]
    fn codec_ids_and_names() {
        for c in [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1] {
            assert_eq!(VideoCodec::from_id(c.id()), Some(c));
            assert_eq!(VideoCodec::from_name(c.name()), Some(c));
            assert_eq!(codec_id_name(c.id()), c.name());
            // The id is the name in ASCII.
            assert_eq!(
                String::from_utf8_lossy(&c.id().to_be_bytes()).trim_start_matches('\0'),
                c.name()
            );
        }
        assert_eq!(codec_id_name(0x0076_7039), "vp9");
        assert_eq!(codec_id_name(0x1234), "0x1234");
    }

    #[test]
    fn orientations() {
        for name in [
            "0", "90", "180", "270", "flip0", "flip90", "flip180", "flip270",
        ] {
            assert_eq!(Orientation::from_name(name).unwrap().as_str(), name);
        }
        assert_eq!(Orientation::from_name("45"), None);
    }
}
