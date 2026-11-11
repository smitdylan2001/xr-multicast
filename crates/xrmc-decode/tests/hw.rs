//! Hardware decoding (FFmpeg hwaccels). Every test is skipped with a message when
//! the machine has no working hardware decoder for the profile.

mod common;

use std::process::Command;
use std::time::Instant;

use common::*;
use xrmc_decode::{
    Backend, Codec, Decoder, DecoderOptions, Frame, PixelFormat, capabilities, create_with,
};

const FRAME_US: u64 = 33_333;

const SOFTWARE: DecoderOptions = DecoderOptions {
    prefer_hw: false,
    threads: 2,
};

fn hw_available(key: &str) -> bool {
    let caps = capabilities();
    let d = caps.details.iter().find(|d| d.key == key).unwrap();
    if !d.caps.hw {
        eprintln!("SKIPPED: no hardware decoder for {key} ({})", caps.backend);
    }
    d.caps.hw
}

fn hw_decoder(codec: Codec) -> Box<dyn Decoder> {
    let dec = create_with(codec, DecoderOptions::default(), Backend::Ffmpeg).unwrap();
    assert!(
        dec.is_hardware(),
        "expected a hardware decoder, got {}",
        dec.backend()
    );
    dec
}

/// Decode all packets; asserts one frame per access unit, out of the push of its
/// own packet (no added latency).
fn decode(dec: &mut dyn Decoder, packets: &[Packet]) -> Vec<Frame> {
    let mut frames = Vec::new();
    for (i, p) in packets.iter().enumerate() {
        let pts = i as u64 * FRAME_US;
        let out = dec
            .push(&p.data, pts, p.key, p.config)
            .unwrap_or_else(|e| panic!("{} packet {i}: {e}", dec.backend()));
        if !p.config {
            assert_eq!(
                out.len(),
                1,
                "{}: packet {i} gave {} frames",
                dec.backend(),
                out.len()
            );
            assert_eq!(out[0].pts, pts);
        }
        frames.extend(out);
    }
    frames
}

/// Samples of a frame as 10-bit values: (Y, U, V) planes, tightly packed.
fn samples10(f: &Frame) -> [Vec<u16>; 3] {
    let (w, h) = (f.width as usize, f.height as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let rd = |p: &xrmc_decode::Plane, i: usize| u16::from_le_bytes([p.data[i], p.data[i + 1]]);
    let mut y = Vec::with_capacity(w * h);
    let mut u = Vec::with_capacity(cw * ch);
    let mut v = Vec::with_capacity(cw * ch);
    let p = &f.planes;
    for row in 0..h {
        for x in 0..w {
            y.push(match f.format {
                PixelFormat::I420 | PixelFormat::Nv12 => {
                    u16::from(p[0].data[row * p[0].stride + x]) << 2
                }
                PixelFormat::I420P10 => rd(&p[0], row * p[0].stride + 2 * x),
                PixelFormat::P010 => rd(&p[0], row * p[0].stride + 2 * x) >> 6,
            });
        }
    }
    for row in 0..ch {
        for x in 0..cw {
            let (a, b) = match f.format {
                PixelFormat::I420 => (
                    u16::from(p[1].data[row * p[1].stride + x]) << 2,
                    u16::from(p[2].data[row * p[2].stride + x]) << 2,
                ),
                PixelFormat::Nv12 => (
                    u16::from(p[1].data[row * p[1].stride + 2 * x]) << 2,
                    u16::from(p[1].data[row * p[1].stride + 2 * x + 1]) << 2,
                ),
                PixelFormat::I420P10 => (
                    rd(&p[1], row * p[1].stride + 2 * x),
                    rd(&p[2], row * p[2].stride + 2 * x),
                ),
                PixelFormat::P010 => (
                    rd(&p[1], row * p[1].stride + 4 * x) >> 6,
                    rd(&p[1], row * p[1].stride + 4 * x + 2) >> 6,
                ),
            };
            u.push(a);
            v.push(b);
        }
    }
    [y, u, v]
}

/// (max, mean) absolute difference in 10-bit units over all samples.
fn diff(a: &Frame, b: &Frame) -> (u16, f64) {
    assert_eq!((a.width, a.height), (b.width, b.height));
    let (sa, sb) = (samples10(a), samples10(b));
    let (mut max, mut sum, mut n) = (0u16, 0u64, 0u64);
    for (pa, pb) in sa.iter().zip(&sb) {
        for (x, y) in pa.iter().zip(pb) {
            let d = x.abs_diff(*y);
            max = max.max(d);
            sum += u64::from(d);
            n += 1;
        }
    }
    (max, sum as f64 / n as f64)
}

struct Case {
    name: &'static str,
    key: &'static str,
    codec: Codec,
    packets: Vec<Packet>,
}

fn cases() -> Vec<Case> {
    let c = |name, key, codec, packets| Case {
        name,
        key,
        codec,
        packets,
    };
    vec![
        c(
            "h264 baseline",
            "h264",
            Codec::H264,
            annexb_packets(&data("h264_baseline.h264"), false),
        ),
        c(
            "h264 high",
            "h264",
            Codec::H264,
            annexb_packets(&data("h264_high.h264"), false),
        ),
        c(
            "h264 full range bt709",
            "h264",
            Codec::H264,
            annexb_packets(&data("h264_fullrange_bt709.h264"), false),
        ),
        c(
            "h264 emulator",
            "h264",
            Codec::H264,
            annexb_packets(&data("emulator_1280x800.h264"), false),
        ),
        c(
            "h265 main",
            "h265",
            Codec::H265,
            annexb_packets(&data("h265_main.h265"), true),
        ),
        c(
            "h265 main10",
            "h265_10",
            Codec::H265,
            annexb_packets(&data("h265_main10.h265"), true),
        ),
        c(
            "h265 emulator",
            "h265",
            Codec::H265,
            annexb_packets(&data("emulator_1280x800.h265"), true),
        ),
        c(
            "av1 8-bit (av1C)",
            "av1",
            Codec::Av1,
            av1_packets(&data("av1_8.obu"), true, false),
        ),
        c(
            "av1 10-bit (raw OBUs)",
            "av1_10",
            Codec::Av1,
            av1_packets(&data("av1_10.obu"), false, true),
        ),
        c(
            "av1 10-bit (av1C)",
            "av1_10",
            Codec::Av1,
            av1_packets(&data("av1_10.obu"), true, true),
        ),
    ]
}

/// Every test stream decodes in hardware (NV12 / P010) to the same pictures as
/// in software. H.264, H.265 and AV1 decoding is exactly specified, so the
/// hardware output must be (practically) bit-exact.
#[test]
fn hw_decodes_every_test_stream_like_software() {
    let mut tested = 0;
    for case in cases() {
        if !hw_available(case.key) {
            continue;
        }
        let mut hw = hw_decoder(case.codec);
        let hw_frames = decode(hw.as_mut(), &case.packets);
        assert!(
            hw.is_hardware(),
            "{}: fell back to {}",
            case.name,
            hw.backend()
        );
        let mut sw = create_with(case.codec, SOFTWARE, Backend::Ffmpeg).unwrap();
        let sw_frames = decode(sw.as_mut(), &case.packets);
        assert_eq!(hw_frames.len(), sw_frames.len(), "{}", case.name);
        let ten = case.key.ends_with("_10");
        let (mut max, mut mean) = (0, 0.0f64);
        for (i, (h, s)) in hw_frames.iter().zip(&sw_frames).enumerate() {
            let want = if ten {
                PixelFormat::P010
            } else {
                PixelFormat::Nv12
            };
            assert_geometry(h, s.width, s.height, want);
            assert_eq!(h.color, s.color, "{} frame {i}", case.name);
            let (m, a) = diff(h, s);
            max = max.max(m);
            mean = mean.max(a);
        }
        eprintln!(
            "{:<24} {} frames via {:<18} vs {:<16} max diff {max} (10-bit units), worst mean {mean:.4}",
            case.name,
            hw_frames.len(),
            hw.backend(),
            sw.backend()
        );
        assert!(
            max <= 8 && mean < 0.05,
            "{}: hardware output differs (max {max}, mean {mean})",
            case.name
        );
        if case.name.contains("emulator")
            || case.name.contains("main")
            || case.name.starts_with("av1")
        {
            hw_frames.iter().take(1).for_each(|f| {
                if f.width == 320 {
                    assert_smpte(f);
                }
            });
        }
        tested += 1;
    }
    eprintln!("{tested} streams compared");
}

/// Decoding starts at a key frame after config, recovers after corruption, and
/// stays in hardware.
#[test]
fn hw_waits_for_key_frame_and_recovers() {
    if !hw_available("h264") {
        return;
    }
    let packets = annexb_packets(&data("h264_high.h264"), false);
    let mut dec = hw_decoder(Codec::H264);
    let keys: Vec<usize> = (0..packets.len()).filter(|&i| packets[i].key).collect();
    // Deltas before the first key frame are dropped.
    for p in &packets[keys[0] + 1..keys[1]] {
        assert!(dec.push(&p.data, 0, p.key, p.config).unwrap().is_empty());
        assert!(dec.waiting_for_key_frame());
    }
    dec.push(&packets[0].data, 0, false, true).unwrap();
    let mut frames = Vec::new();
    for (i, p) in packets[keys[1]..].iter().enumerate() {
        let mut d = p.data.clone();
        if i == 2 {
            let n = d.len();
            d[6..n]
                .iter_mut()
                .enumerate()
                .for_each(|(k, b)| *b = (k as u8).wrapping_mul(97));
        }
        if let Ok(out) = dec.push(&d, i as u64, p.key, p.config) {
            frames.extend(out);
        }
    }
    assert!(
        dec.is_hardware(),
        "a corrupt packet must not switch to software"
    );
    assert!(!frames.is_empty());
    assert_smpte(frames.last().unwrap());
}

/// A stream the hardware can't decode (H.264 High 10 on VA-API) switches to
/// software at the first key frame without losing it.
#[test]
fn unsupported_profile_falls_back_to_software_without_losing_frames() {
    if !hw_available("h264") {
        return;
    }
    let packets = annexb_packets(&data("h264_high10.h264"), false);
    let mut dec = hw_decoder(Codec::H264);
    let mut frames = Vec::new();
    for (i, p) in packets.iter().enumerate() {
        frames.extend(dec.push(&p.data, i as u64, p.key, p.config).unwrap());
    }
    eprintln!(
        "High 10 decoded by {} (hardware: {})",
        dec.backend(),
        dec.is_hardware()
    );
    assert_eq!(frames.len(), 4);
    for f in &frames {
        assert!(is_ten_bit(f.format));
        assert_smpte(f);
    }
}

/// 12 simultaneous 1280x720 hardware decoders (as with a room of headsets), each
/// on its own thread: every frame comes out, nothing stalls or falls back.
#[test]
fn twelve_concurrent_hw_decoders_720p() {
    const N: usize = 12;
    const FRAMES: u32 = 300;
    let mut kinds = Vec::new();
    for (kind, key) in [(Synth::H264, "h264"), (Synth::H265, "h265")] {
        if hw_available(key) {
            match synthetic_720p(kind, FRAMES) {
                Some(s) => kinds.push((kind, synthetic_packets(kind, &s))),
                None => eprintln!("SKIPPED {kind:?}: needs gst-launch-1.0 with an encoder"),
            }
        }
    }
    if kinds.is_empty() {
        return;
    }
    let start = Instant::now();
    let handles: Vec<_> = (0..N)
        .map(|i| {
            let (kind, packets) = kinds[i % kinds.len()].clone();
            std::thread::spawn(move || {
                let codec = if kind == Synth::H264 {
                    Codec::H264
                } else {
                    Codec::H265
                };
                let mut dec = hw_decoder(codec);
                let mut n = 0;
                let mut worst = std::time::Duration::ZERO;
                for (k, p) in packets.iter().enumerate() {
                    let t = Instant::now();
                    let out = dec.push(&p.data, k as u64, p.key, p.config).unwrap();
                    worst = worst.max(t.elapsed());
                    if !p.config {
                        assert_eq!(out.len(), 1, "decoder {i} packet {k}");
                        assert_eq!((out[0].width, out[0].height), (1280, 720));
                    }
                    n += out.len();
                }
                assert!(
                    dec.is_hardware(),
                    "decoder {i} fell back to {}",
                    dec.backend()
                );
                (n, worst, dec.backend().to_owned())
            })
        })
        .collect();
    let mut total = 0;
    for (i, h) in handles.into_iter().enumerate() {
        let (n, worst, backend) = h.join().unwrap();
        assert_eq!(n, FRAMES as usize, "decoder {i}");
        eprintln!("decoder {i:>2} ({backend}): {n} frames, slowest push {worst:?}");
        total += n;
    }
    let secs = start.elapsed().as_secs_f64();
    eprintln!(
        "{N} concurrent 1280x720 hardware decoders: {total} frames in {secs:.2} s ({:.0} fps total, {:.0} fps each)",
        total as f64 / secs,
        total as f64 / secs / N as f64
    );
}

/// Environment overrides and broken devices fall back to software without
/// panicking. The probe runs once per process, so each case runs this test
/// binary again with the environment set (see `env_case`).
#[test]
fn env_overrides_and_broken_devices_fall_back_to_software() {
    if xrmc_decode::ffmpeg_version().is_none() {
        eprintln!("SKIPPED: no FFmpeg");
        return;
    }
    let exe = std::env::current_exe().unwrap();
    type Env = &'static [(&'static str, &'static str)];
    let cases: [(&str, Env); 4] = [
        ("disabled", &[("XRMC_HWDEC", "0")]),
        ("unknown type", &[("XRMC_HWDEC", "bogus")]),
        (
            "invalid render node",
            &[("XRMC_HWDEC", "vaapi"), ("XRMC_HWDEC_DEVICE", "/dev/null")],
        ),
        (
            "missing render node",
            &[("XRMC_HWDEC_DEVICE", "/dev/dri/renderD999")],
        ),
    ];
    for (name, env) in cases {
        let mut cmd = Command::new(&exe);
        cmd.args([
            "env_case",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("XRMC_TEST_ENV_CASE", name);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout) + String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{name}: {text}");
        assert!(text.contains("env case ok"), "{name}: {text}");
        eprintln!("{name}: ok");
    }
}

/// Child process of `env_overrides_and_broken_devices_fall_back_to_software`.
#[test]
#[ignore]
fn env_case() {
    let Ok(case) = std::env::var("XRMC_TEST_ENV_CASE") else {
        return;
    };
    let caps = capabilities();
    eprintln!("{case}: {}", caps.backend);
    assert!(
        caps.details
            .iter()
            .all(|d| !d.caps.hw && d.hw_api.is_none()),
        "{case}: {caps:#?}"
    );
    assert!(!caps.backend.contains("hardware"));
    for (codec, packets) in [
        (Codec::H264, annexb_packets(&data("h264_high.h264"), false)),
        (Codec::H265, annexb_packets(&data("h265_main.h265"), true)),
    ] {
        let mut dec = create_with(codec, DecoderOptions::default(), Backend::Ffmpeg).unwrap();
        assert!(!dec.is_hardware(), "{case}: {}", dec.backend());
        let frames = decode(dec.as_mut(), &packets);
        assert_eq!(frames.len(), 20);
        frames.iter().for_each(assert_smpte);
    }
    println!("env case ok");
}
