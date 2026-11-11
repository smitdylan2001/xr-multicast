//! End-to-end decoding of recorded bitstreams through both backends.
//! FFmpeg tests are skipped (with a message) when no supported libavcodec is found.

mod common;

use common::*;
use xrmc_decode::{
    Backend, Codec, ColorInfo, Decoder, DecoderOptions, Frame, PixelFormat, capabilities, create,
    create_with,
};

const FRAME_US: u64 = 33_333;

fn ffmpeg(codec: Codec) -> Option<Box<dyn Decoder>> {
    match create_with(codec, DecoderOptions::default(), Backend::Ffmpeg) {
        Ok(d) => Some(d),
        Err(e) => {
            eprintln!(
                "SKIPPED: no FFmpeg decoder for {codec:?} ({e}); FFmpeg: {:?}",
                xrmc_decode::ffmpeg_version()
            );
            None
        }
    }
}

fn openh264() -> Box<dyn Decoder> {
    create_with(Codec::H264, DecoderOptions::default(), Backend::OpenH264)
        .expect("OpenH264 is built in on Linux")
}

/// Feed all packets; returns the frames. Panics on decoder errors.
fn decode(dec: &mut dyn Decoder, packets: &[Packet]) -> Vec<Frame> {
    let mut frames = Vec::new();
    for (i, p) in packets.iter().enumerate() {
        let out = dec
            .push(&p.data, i as u64 * FRAME_US, p.key, p.config)
            .unwrap_or_else(|e| panic!("packet {i}: {e}"));
        frames.extend(out);
    }
    frames
}

fn frame_packets(packets: &[Packet]) -> usize {
    packets.iter().filter(|p| !p.config).count()
}

fn check_smpte_stream(dec: &mut dyn Decoder, packets: &[Packet], format: PixelFormat) {
    let frames = decode(dec, packets);
    assert_eq!(
        frames.len(),
        frame_packets(packets),
        "one frame per access unit ({})",
        dec.backend()
    );
    for f in &frames {
        assert_geometry(f, 320, 240, format);
        assert!(
            !f.color.bt709 && !f.color.full_range,
            "SD stream: BT.601 limited, got {:?}",
            f.color
        );
        assert_smpte(f);
    }
    // Low latency: each frame comes out of the push of its own packet, in order.
    let mut pts: Vec<u64> = frames.iter().map(|f| f.pts).collect();
    let sorted = {
        pts.dedup();
        let mut s = pts.clone();
        s.sort();
        s
    };
    assert_eq!(pts, sorted);
    assert_eq!(pts.len(), frames.len());
}

fn h264_streams() -> [(&'static str, Vec<Packet>); 2] {
    [
        (
            "baseline",
            annexb_packets(&data("h264_baseline.h264"), false),
        ),
        ("high", annexb_packets(&data("h264_high.h264"), false)),
    ]
}

#[test]
fn packetizer_produces_scrcpy_style_packets() {
    let p = annexb_packets(&data("h264_baseline.h264"), false);
    assert!(p[0].config && p[1].key && !p[1].config);
    assert_eq!(frame_packets(&p), 20);
    assert_eq!(p.iter().filter(|p| p.key).count(), 3); // key-int-max=8
    let p = annexb_packets(&data("h265_main.h265"), true);
    assert!(p[0].config && p[1].key);
    assert_eq!(frame_packets(&p), 20);
    let p = av1_packets(&data("av1_8.obu"), true, false);
    assert!(p[0].config && p[1].key);
    assert_eq!(frame_packets(&p), 20);
}

#[test]
fn openh264_decodes_baseline_and_high() {
    for (name, packets) in h264_streams() {
        eprintln!("openh264 {name}");
        let mut dec = openh264();
        assert_eq!(dec.backend(), "openh264");
        check_smpte_stream(dec.as_mut(), &packets, PixelFormat::I420);
    }
}

#[test]
fn ffmpeg_decodes_h264_baseline_and_high() {
    for (name, packets) in h264_streams() {
        let Some(mut dec) = ffmpeg(Codec::H264) else {
            return;
        };
        eprintln!("{} {name}", dec.backend());
        check_smpte_stream(dec.as_mut(), &packets, PixelFormat::I420);
    }
}

#[test]
fn ffmpeg_decodes_h265_main() {
    let Some(mut dec) = ffmpeg(Codec::H265) else {
        return;
    };
    check_smpte_stream(
        dec.as_mut(),
        &annexb_packets(&data("h265_main.h265"), true),
        PixelFormat::I420,
    );
}

#[test]
fn ffmpeg_decodes_h265_main10() {
    let Some(mut dec) = ffmpeg(Codec::H265) else {
        return;
    };
    check_smpte_stream(
        dec.as_mut(),
        &annexb_packets(&data("h265_main10.h265"), true),
        PixelFormat::I420P10,
    );
}

#[test]
fn ffmpeg_decodes_av1_8bit_with_av1c_config() {
    let Some(mut dec) = ffmpeg(Codec::Av1) else {
        return;
    };
    check_smpte_stream(
        dec.as_mut(),
        &av1_packets(&data("av1_8.obu"), true, false),
        PixelFormat::I420,
    );
}

#[test]
fn ffmpeg_decodes_av1_10bit_with_raw_obu_config() {
    let Some(mut dec) = ffmpeg(Codec::Av1) else {
        return;
    };
    check_smpte_stream(
        dec.as_mut(),
        &av1_packets(&data("av1_10.obu"), false, true),
        PixelFormat::I420P10,
    );
}

#[test]
fn ffmpeg_decodes_av1_10bit_with_av1c_config() {
    let Some(mut dec) = ffmpeg(Codec::Av1) else {
        return;
    };
    check_smpte_stream(
        dec.as_mut(),
        &av1_packets(&data("av1_10.obu"), true, true),
        PixelFormat::I420P10,
    );
}

/// Without the config packet being prepended, the first key frame can't decode.
#[test]
fn config_is_required_and_prepended() {
    let packets = annexb_packets(&data("h264_baseline.h264"), false);
    let mut dec = openh264();
    let frames = decode(dec.as_mut(), &packets[..2]);
    assert_eq!(frames.len(), 1, "config + key frame -> one frame");
    let Some(mut dec) = ffmpeg(Codec::H264) else {
        return;
    };
    assert_eq!(decode(dec.as_mut(), &packets[..2]).len(), 1);
}

/// Both H.264 decoders are bit-exact, so FFmpeg's low-latency settings
/// (`low_delay`, `flags2=fast`) must not change the pictures.
#[test]
fn openh264_and_ffmpeg_bit_identical_on_x264_streams() {
    for (name, packets) in h264_streams() {
        let a = decode(openh264().as_mut(), &packets);
        let Some(mut ff) = ffmpeg(Codec::H264) else {
            return;
        };
        let b = decode(ff.as_mut(), &packets);
        assert_eq!(a.len(), b.len());
        for (i, (x, y)) in a.iter().zip(&b).enumerate() {
            assert!(
                x == y,
                "{name}: frame {i} differs between OpenH264 and FFmpeg"
            );
        }
    }
}

/// Colour signalling in the SPS (full range, BT.709 on an SD picture) wins over
/// the size-based default, on both backends.
#[test]
fn signalled_full_range_bt709_is_reported() {
    let packets = annexb_packets(&data("h264_fullrange_bt709.h264"), false);
    let mut decoders = vec![openh264()];
    decoders.extend(ffmpeg(Codec::H264));
    for mut dec in decoders {
        let frames = decode(dec.as_mut(), &packets);
        assert_eq!(frames.len(), 4);
        for f in &frames {
            assert_geometry(f, 320, 240, PixelFormat::I420);
            assert_eq!(
                f.color,
                ColorInfo {
                    bt709: true,
                    full_range: true
                },
                "{}",
                dec.backend()
            );
        }
    }
}

/// Real Android emulator recording (scrcpy, 1280x800 H.264 Constrained Baseline,
/// BT.709): both backends must produce bit-identical pictures.
#[test]
fn emulator_h264_openh264_and_ffmpeg_agree() {
    let packets = annexb_packets(&data("emulator_1280x800.h264"), false);
    assert_eq!(frame_packets(&packets), 28);
    let mut oh = openh264();
    let a = decode(oh.as_mut(), &packets);
    assert_eq!(a.len(), 28);
    for f in &a {
        assert_geometry(f, 1280, 800, PixelFormat::I420);
        assert!(f.color.bt709 && !f.color.full_range, "{:?}", f.color);
    }
    // Not a blank screen.
    let y = &a[0].planes[0].data;
    let (min, max) = (y.iter().min().unwrap(), y.iter().max().unwrap());
    assert!(max - min > 100, "luma range {min}..{max}");

    let Some(mut ff) = ffmpeg(Codec::H264) else {
        return;
    };
    let b = decode(ff.as_mut(), &packets);
    assert_eq!(b.len(), 28);
    for (i, (x, y)) in a.iter().zip(&b).enumerate() {
        assert_eq!(x.color, y.color, "frame {i}");
        assert!(
            x.planes == y.planes,
            "frame {i} differs between OpenH264 and FFmpeg"
        );
    }
}

/// Real Android emulator recording, H.265 (VPS/SPS/PPS sent twice, as by scrcpy).
#[test]
fn emulator_h265_ffmpeg() {
    let Some(mut dec) = ffmpeg(Codec::H265) else {
        return;
    };
    let packets = annexb_packets(&data("emulator_1280x800.h265"), true);
    let frames = decode(dec.as_mut(), &packets);
    assert_eq!(frames.len(), frame_packets(&packets));
    assert_eq!(frames.len(), 39);
    for f in &frames {
        assert_geometry(f, 1280, 800, PixelFormat::I420);
        assert!(f.color.bt709);
    }
}

/// Start in the middle of a GOP: nothing comes out until the next key frame.
fn check_no_frames_before_key(dec: &mut dyn Decoder, packets: &[Packet]) {
    // Config, then the key frame's GOP skipped: begin at delta frames.
    let config = packets.iter().position(|p| p.config).unwrap();
    let first_key = packets.iter().position(|p| p.key).unwrap();
    let second_key = first_key + 1 + packets[first_key + 1..].iter().position(|p| p.key).unwrap();
    let mut early = Vec::new();
    for (i, p) in packets[first_key + 1..second_key].iter().enumerate() {
        early.extend(dec.push(&p.data, i as u64, p.key, p.config).unwrap());
        assert!(dec.waiting_for_key_frame());
    }
    assert!(
        early.is_empty(),
        "{}: {} frames before the first key frame",
        dec.backend(),
        early.len()
    );
    dec.push(&packets[config].data, 0, false, true).unwrap();
    let frames = decode(dec, &packets[second_key..]);
    assert!(!dec.waiting_for_key_frame());
    assert_eq!(
        frames.len(),
        frame_packets(&packets[second_key..]),
        "{}",
        dec.backend()
    );
    assert_smpte(&frames[0]);
}

#[test]
fn no_frames_before_first_key_frame() {
    let packets = annexb_packets(&data("h264_high.h264"), false);
    check_no_frames_before_key(openh264().as_mut(), &packets);
    if let Some(mut d) = ffmpeg(Codec::H264) {
        check_no_frames_before_key(d.as_mut(), &packets);
    }
    if let Some(mut d) = ffmpeg(Codec::H265) {
        check_no_frames_before_key(d.as_mut(), &annexb_packets(&data("h265_main.h265"), true));
    }
    if let Some(mut d) = ffmpeg(Codec::Av1) {
        check_no_frames_before_key(d.as_mut(), &av1_packets(&data("av1_8.obu"), true, false));
    }
}

/// Corrupt a delta frame: the decoder either reports an error (then drops deltas
/// until the next key frame) or conceals it; either way the next key frame decodes
/// to the right picture and decoding continues.
fn check_recovery(dec: &mut dyn Decoder, packets: &[Packet], corrupt: impl Fn(&mut Vec<u8>)) {
    let keys: Vec<usize> = packets
        .iter()
        .enumerate()
        .filter(|(_, p)| p.key)
        .map(|(i, _)| i)
        .collect();
    let bad = keys[0] + 2;
    assert!(bad < keys[1]);
    let mut errors = 0;
    let mut after_key = Vec::new();
    for (i, p) in packets.iter().enumerate() {
        let mut data = p.data.clone();
        if i == bad {
            corrupt(&mut data);
        }
        match dec.push(&data, i as u64 * FRAME_US, p.key, p.config) {
            Ok(frames) => {
                if i > bad && i < keys[1] && dec.waiting_for_key_frame() {
                    assert!(frames.is_empty(), "frames while waiting for a key frame");
                }
                if i >= keys[1] {
                    after_key.extend(frames);
                }
            }
            Err(e) => {
                assert!(
                    i >= bad && i < keys[1],
                    "unexpected error at packet {i}: {e}"
                );
                assert!(dec.waiting_for_key_frame());
                errors += 1;
            }
        }
    }
    eprintln!(
        "{}: {errors} decoder error(s) after corruption",
        dec.backend()
    );
    assert_eq!(after_key.len(), frame_packets(&packets[keys[1]..]));
    for f in &after_key {
        assert_smpte(f);
    }
}

fn garbage(d: &mut [u8]) {
    // Keep the start code and NAL header, scramble the slice.
    let start = 6.min(d.len());
    for (i, b) in d[start..].iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(97).wrapping_add(13);
    }
}

#[test]
fn recovers_after_corrupt_packet() {
    let packets = annexb_packets(&data("h264_high.h264"), false);
    check_recovery(openh264().as_mut(), &packets, |d| garbage(d));
    check_recovery(openh264().as_mut(), &packets, |d| d.truncate(d.len() / 3));
    if let Some(mut d) = ffmpeg(Codec::H264) {
        check_recovery(d.as_mut(), &packets, |d| garbage(d));
    }
    if let Some(mut d) = ffmpeg(Codec::H265) {
        check_recovery(
            d.as_mut(),
            &annexb_packets(&data("h265_main.h265"), true),
            |d| garbage(d),
        );
    }
    if let Some(mut d) = ffmpeg(Codec::Av1) {
        check_recovery(
            d.as_mut(),
            &av1_packets(&data("av1_8.obu"), true, false),
            |d| {
                let n = d.len();
                d[n / 2..].iter_mut().for_each(|b| *b ^= 0x5a);
            },
        );
    }
}

/// A decoder error puts the decoder into "wait for key frame" mode: following
/// delta frames are dropped, and the next key frame decodes cleanly.
fn check_error_waits_for_key(
    dec: &mut dyn Decoder,
    packets: &[Packet],
    corrupt: impl Fn(&mut Vec<u8>),
) {
    let keys: Vec<usize> = packets
        .iter()
        .enumerate()
        .filter(|(_, p)| p.key)
        .map(|(i, _)| i)
        .collect();
    decode(dec, &packets[..keys[0] + 2]);
    let mut bad = packets[keys[0] + 2].data.clone();
    corrupt(&mut bad);
    let err = dec.push(&bad, 0, false, false);
    assert!(err.is_err(), "{}: corrupt packet must fail", dec.backend());
    assert!(dec.waiting_for_key_frame());
    for p in &packets[keys[0] + 3..keys[1]] {
        assert!(
            dec.push(&p.data, 0, p.key, p.config).unwrap().is_empty(),
            "delta frame decoded while waiting for a key frame"
        );
    }
    let frames = decode(dec, &packets[keys[1]..]);
    assert!(!dec.waiting_for_key_frame());
    assert_eq!(frames.len(), frame_packets(&packets[keys[1]..]));
    frames.iter().for_each(assert_smpte);
}

#[test]
fn error_waits_for_next_key_frame() {
    let packets = annexb_packets(&data("h264_high.h264"), false);
    // OpenH264 rejects a truncated slice.
    check_error_waits_for_key(openh264().as_mut(), &packets, |d| d.truncate(d.len() / 3));
    // FFmpeg rejects a slice that references a PPS that doesn't exist.
    if let Some(mut dec) = ffmpeg(Codec::H264) {
        check_error_waits_for_key(dec.as_mut(), &packets, |d| {
            *d = vec![0, 0, 0, 1, 0x41, 0x9f, 0xff, 0xff, 0xff, 0xff, 0, 0x11]
        });
    }
}

#[test]
fn capabilities_report_what_decodes() {
    let caps = capabilities();
    eprintln!("{caps:#?}");
    assert!(caps.h264.any, "OpenH264 is always available on Linux");
    assert!(caps.backend.contains("OpenH264"));
    assert!(!caps.h264.hw && !caps.h265.hw && !caps.av1.hw);
    if xrmc_decode::ffmpeg_version().is_some() {
        assert!(caps.backend.contains("FFmpeg"));
        assert_eq!(caps.h265.any, ffmpeg(Codec::H265).is_some());
        assert_eq!(caps.av1.any, ffmpeg(Codec::Av1).is_some());
        assert_eq!(caps.h265_10.any, caps.h265.any);
    } else {
        assert!(!caps.h265.any && !caps.av1.any);
    }
}

#[test]
fn create_prefers_ffmpeg_then_openh264() {
    let dec = create(Codec::H264, DecoderOptions::default()).unwrap();
    if xrmc_decode::ffmpeg_version().is_some() {
        assert_eq!(dec.backend(), "ffmpeg:h264");
    } else {
        assert_eq!(dec.backend(), "openh264");
    }
    assert!(!dec.is_hardware());
    assert!(dec.waiting_for_key_frame());
    match create_with(Codec::H265, DecoderOptions::default(), Backend::OpenH264) {
        Err(xrmc_decode::DecodeError::Unsupported(Codec::H265)) => {}
        other => panic!(
            "OpenH264 can't decode H.265: {:?}",
            other.map(|d| d.backend().to_owned())
        ),
    }
}

#[test]
fn decoders_are_send() {
    fn is_send<T: Send>(_: &T) {}
    let dec = create(Codec::H264, DecoderOptions::default()).unwrap();
    is_send(&dec);
    let packets = annexb_packets(&data("h264_baseline.h264"), false);
    let frames = std::thread::spawn(move || {
        let mut dec = dec;
        decode(dec.as_mut(), &packets).len()
    })
    .join()
    .unwrap();
    assert_eq!(frames, 20);
}
