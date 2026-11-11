//! Decode throughput and memory (ignored by default):
//! `cargo test -p xrmc-decode --release --test bench -- --ignored --nocapture`

mod common;

use std::time::Instant;

use common::*;
use xrmc_decode::{Backend, Codec, DecoderOptions, create_with};

/// 1280x720 H.264 High, 600 frames of moving SMPTE bars + clock at ~8 Mbit/s
/// (like scrcpy's default), generated with GStreamer. `None` if unavailable.
fn synthetic_720p() -> Option<Vec<u8>> {
    let path = std::env::temp_dir().join("xrmc-decode-bench-720p.h264");
    if !path.exists() {
        let status = std::process::Command::new("gst-launch-1.0")
            .args([
                "-q",
                "videotestsrc",
                "pattern=smpte",
                "horizontal-speed=4",
                "num-buffers=600",
                "!",
            ])
            .args([
                "video/x-raw,width=1280,height=720,framerate=60/1,format=I420",
                "!",
                "timeoverlay",
                "font-desc=Sans 60",
                "!",
            ])
            .args([
                "x264enc",
                "tune=zerolatency",
                "bframes=0",
                "key-int-max=600",
                "bitrate=8000",
                "speed-preset=veryfast",
                "!",
            ])
            .args([
                "video/x-h264,profile=high,stream-format=byte-stream,alignment=au",
                "!",
                "filesink",
            ])
            .arg(format!("location={}", path.display()))
            .status()
            .ok()?;
        if !status.success() {
            return None;
        }
    }
    std::fs::read(path).ok()
}

fn bench(label: &str, packets: &[Packet], backend: Backend, threads: u32, rounds: usize) {
    let opts = DecoderOptions {
        prefer_hw: false,
        threads,
    };
    let Ok(mut dec) = create_with(Codec::H264, opts, backend) else {
        println!("{label:<34} unavailable");
        return;
    };
    let start = Instant::now();
    let mut frames = 0;
    for _ in 0..rounds {
        for (i, p) in packets.iter().enumerate() {
            frames += dec.push(&p.data, i as u64, p.key, p.config).unwrap().len();
        }
    }
    let secs = start.elapsed().as_secs_f64();
    println!(
        "{label:<34} {:>7.0} fps  ({frames} frames in {secs:.2} s, {})",
        frames as f64 / secs,
        dec.backend()
    );
}

fn rss_kib() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4)
}

#[test]
#[ignore]
fn decode_throughput() {
    let emu = annexb_packets(&data("emulator_1280x800.h264"), false);
    println!("\n-- Android emulator recording, 1280x800 Constrained Baseline (28 frames x 20)");
    bench("FFmpeg, 1 thread", &emu, Backend::Ffmpeg, 1, 20);
    bench("FFmpeg, 2 threads (slice)", &emu, Backend::Ffmpeg, 2, 20);
    bench("OpenH264", &emu, Backend::OpenH264, 1, 20);

    match synthetic_720p() {
        Some(stream) => {
            let p = annexb_packets(&stream, false);
            println!(
                "-- synthetic 1280x720 High, {} frames, {} kB",
                p.iter().filter(|p| !p.config).count(),
                stream.len() / 1000
            );
            bench("FFmpeg, 1 thread", &p, Backend::Ffmpeg, 1, 2);
            bench("FFmpeg, 2 threads (slice)", &p, Backend::Ffmpeg, 2, 2);
            bench("OpenH264", &p, Backend::OpenH264, 1, 2);
        }
        None => println!("-- synthetic 720p skipped (gst-launch-1.0 with x264enc not available)"),
    }
}

#[test]
#[ignore]
fn memory_per_decoder() {
    let Some(stream) = synthetic_720p() else {
        println!("skipped: needs gst-launch-1.0 with x264enc");
        return;
    };
    let packets = annexb_packets(&stream, false);
    for backend in [Backend::Ffmpeg, Backend::OpenH264] {
        let before = rss_kib().unwrap_or(0);
        let mut decoders = Vec::new();
        for _ in 0..8 {
            let Ok(mut d) = create_with(
                Codec::H264,
                DecoderOptions {
                    prefer_hw: false,
                    threads: 2,
                },
                backend,
            ) else {
                println!("{backend:?}: unavailable");
                break;
            };
            for (i, p) in packets.iter().take(31).enumerate() {
                d.push(&p.data, i as u64, p.key, p.config).unwrap();
            }
            decoders.push(d);
        }
        if decoders.is_empty() {
            continue;
        }
        let after = rss_kib().unwrap_or(0);
        println!(
            "{backend:?}: ~{:.1} MiB RSS per 1280x720 decoder (8 decoders, after 30 frames each)",
            (after.saturating_sub(before)) as f64 / 1024.0 / decoders.len() as f64
        );
    }
}
