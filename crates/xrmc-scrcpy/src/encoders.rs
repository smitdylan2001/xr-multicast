//! Video encoder discovery (`list_encoders=true`).

use std::time::Duration;

use xrmc_adb::AdbDevice;

use crate::options::{VideoCodec, server_command};
use crate::{Error, REMOTE_LIST_SERVER_PATH};

/// How long pushing the server jar may take.
const PUSH_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the listing may take.
const LIST_TIMEOUT: Duration = Duration::from_secs(20);

/// One video encoder on the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderInfo {
    /// Codec name as scrcpy reports it (`h264`, `h265`, `av1`, `vp8`, ...).
    pub codec: String,
    /// MediaCodec encoder name (for `video_encoder=`).
    pub name: String,
    /// Hardware (or hybrid) encoder.
    pub hw: bool,
    /// An alias of another encoder in the list.
    pub alias: bool,
}

impl EncoderInfo {
    /// The codec, if it is one the app can request.
    pub fn video_codec(&self) -> Option<VideoCodec> {
        VideoCodec::from_name(&self.codec)
    }
}

/// Parse scrcpy's "List of video encoders" output. Lines look like:
///
/// ```text
///     --video-codec=h265 --video-encoder=c2.qti.hevc.encoder      (hw) [vendor]
/// ```
///
/// The `(hw)`/`(sw)`/`(hybrid)` marker only exists on Android 10+; older
/// devices are classified by the well-known software encoder name prefixes
/// (`c2.android.`, `OMX.google.`).
pub fn parse_encoder_list(text: &str) -> Vec<EncoderInfo> {
    const CODEC: &str = "--video-codec=";
    const ENCODER: &str = "--video-encoder=";
    let mut list = Vec::new();
    let mut from = 0;
    while let Some(rel) = text[from..].find(CODEC) {
        let start = from + rel;
        from = start + 1;
        let codec_start = start + CODEC.len();
        let codec_end = codec_start
            + text[codec_start..]
                .bytes()
                .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
                .count();
        let ws_end = codec_end + whitespace_len(&text[codec_end..]);
        if codec_end == codec_start || ws_end == codec_end || !text[ws_end..].starts_with(ENCODER) {
            continue;
        }
        let name_start = ws_end + ENCODER.len();
        let name_end = name_start
            + text[name_start..]
                .chars()
                .take_while(|c| !c.is_whitespace())
                .map(char::len_utf8)
                .sum::<usize>();
        if name_end == name_start {
            continue;
        }
        let rest_end = text[name_end..]
            .find('\n')
            .map_or(text.len(), |p| name_end + p);
        let (codec, name, rest) = (
            &text[codec_start..codec_end],
            &text[name_start..name_end],
            &text[name_end..rest_end],
        );
        let hw = if rest.contains("(hw)") || rest.contains("(hybrid)") {
            true
        } else if rest.contains("(sw)") {
            false
        } else {
            let lower = name.to_ascii_lowercase();
            !(lower.starts_with("c2.android.") || lower.starts_with("omx.google."))
        };
        list.push(EncoderInfo {
            codec: codec.to_owned(),
            name: name.to_owned(),
            hw,
            alias: rest.contains("alias for"),
        });
        from = rest_end;
    }
    list
}

fn whitespace_len(s: &str) -> usize {
    s.chars()
        .take_while(|c| c.is_whitespace())
        .map(char::len_utf8)
        .sum()
}

/// The command that runs scrcpy-server in encoder-listing mode from [`REMOTE_LIST_SERVER_PATH`].
pub fn list_encoders_command() -> String {
    server_command(
        REMOTE_LIST_SERVER_PATH,
        &["list_encoders=true", "cleanup=true", "log_level=info"],
    )
}

/// Ask the device which video encoders it has: pushes the server jar
/// (`jar` = the scrcpy-server v5.0.1 file) to a separate path from the
/// streaming one (the server deletes its own jar on exit), runs it in list
/// mode and parses the output.
pub async fn list_encoders<D: AdbDevice>(
    device: &D,
    jar: &[u8],
) -> Result<Vec<EncoderInfo>, Error> {
    tokio::time::timeout(
        PUSH_TIMEOUT,
        device.push(jar, REMOTE_LIST_SERVER_PATH, 0o644),
    )
    .await
    .map_err(|_| Error::Timeout("pushing scrcpy-server"))??;
    let out = tokio::time::timeout(LIST_TIMEOUT, device.shell(&list_encoders_command()))
        .await
        .map_err(|_| Error::Timeout("listing encoders"))??;
    let text = String::from_utf8_lossy(&out);
    let list = parse_encoder_list(&text);
    if list.is_empty() {
        let last = text
            .trim()
            .lines()
            .last()
            .unwrap_or_default()
            .trim()
            .to_owned();
        return Err(Error::NoEncoders(last));
    }
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Port of 'parses scrcpy encoder list' (test/stream-parser.test.js).
    #[test]
    fn parses_scrcpy_encoder_list() {
        let out = "[server] INFO: Device: [Oculus] oculus Quest 3 (Android 14)
[server] INFO: List of video encoders:
    --video-codec=h264 --video-encoder=c2.qti.avc.encoder                (hw) [vendor]
    --video-codec=h264 --video-encoder=c2.android.avc.encoder            (sw)
    --video-codec=h265 --video-encoder=c2.qti.hevc.encoder               (hw) [vendor]
    --video-codec=h265 --video-encoder=OMX.qcom.video.encoder.hevc       (hw) [vendor] (alias for c2.qti.hevc.encoder)
    --video-codec=av1 --video-encoder=c2.android.av1.encoder             (sw)
    --video-codec=h264 --video-encoder=OMX.google.h264.encoder
[server] INFO: List of audio encoders:
    --audio-codec=opus --audio-encoder=c2.android.opus.encoder           (sw)";
        let list = parse_encoder_list(out);
        let got: Vec<_> = list
            .iter()
            .map(|e| (e.codec.as_str(), e.name.as_str(), e.hw, e.alias))
            .collect();
        assert_eq!(
            got,
            [
                ("h264", "c2.qti.avc.encoder", true, false),
                ("h264", "c2.android.avc.encoder", false, false),
                ("h265", "c2.qti.hevc.encoder", true, false),
                ("h265", "OMX.qcom.video.encoder.hevc", true, true),
                ("av1", "c2.android.av1.encoder", false, false),
                ("h264", "OMX.google.h264.encoder", false, false),
            ]
        );
        assert_eq!(list[2].video_codec(), Some(VideoCodec::H265));
    }

    #[test]
    fn old_android_name_heuristics_and_crlf() {
        let out = "    --video-codec=h264 --video-encoder=OMX.qcom.video.encoder.avc\r\n\
                   \t--video-codec=h264 --video-encoder=c2.Android.avc.encoder\r\n\
                   --video-codec=vp8 --video-encoder=c2.exynos.vp8.encoder (hybrid)\r\n\
                   --video-codec=h264 junk --video-codec=h265 --video-encoder=x\n\
                   --video-codec= --video-encoder=y\n\
                   --video-codec=h264 --video-encoder=\n";
        let got: Vec<_> = parse_encoder_list(out)
            .into_iter()
            .map(|e| (e.codec, e.name, e.hw))
            .collect();
        assert_eq!(
            got,
            [
                ("h264".into(), "OMX.qcom.video.encoder.avc".into(), true),
                ("h264".into(), "c2.Android.avc.encoder".into(), false),
                ("vp8".into(), "c2.exynos.vp8.encoder".into(), true),
                ("h265".into(), "x".into(), true),
            ]
        );
        assert!(parse_encoder_list("").is_empty());
        assert!(parse_encoder_list("--video-codec=").is_empty());
    }

    #[test]
    fn list_command() {
        assert_eq!(
            list_encoders_command(),
            "CLASSPATH=/data/local/tmp/xrmc-scrcpy-list.jar app_process / com.genymobile.scrcpy.Server 5.0.1 \
             list_encoders=true cleanup=true log_level=info"
        );
    }
}
