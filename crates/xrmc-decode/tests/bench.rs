//! Decode throughput and memory (ignored by default):
//! `cargo test -p xrmc-decode --release --test bench -- --ignored --nocapture`

mod common;

use std::time::Instant;

use common::*;
use xrmc_decode::{Backend, Codec, DecoderOptions, PixelFormat, capabilities, create_with};

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

/// CPU time (ns) used by all threads of this process so far (Linux).
fn cpu_ns() -> Option<u64> {
    let mut total = 0;
    for task in std::fs::read_dir("/proc/self/task").ok()? {
        let stat = std::fs::read_to_string(task.ok()?.path().join("schedstat")).ok()?;
        total += stat.split_whitespace().next()?.parse::<u64>().ok()?;
    }
    Some(total)
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

    match synthetic_720p(Synth::H264, 600) {
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
    let Some(stream) = synthetic_720p(Synth::H264, 600) else {
        println!("skipped: needs gst-launch-1.0 with x264enc");
        return;
    };
    let packets = annexb_packets(&stream, false);
    let runs = [
        ("FFmpeg software", Backend::Ffmpeg, false),
        ("FFmpeg hardware", Backend::Ffmpeg, true),
        ("OpenH264", Backend::OpenH264, false),
    ];
    // Probe first so the shared probe device isn't counted.
    let _ = capabilities();
    for (label, backend, hw) in runs {
        if hw && !capabilities().h264.hw {
            println!("{label}: unavailable");
            continue;
        }
        let before = rss_kib().unwrap_or(0);
        let mut decoders = Vec::new();
        for _ in 0..8 {
            let Ok(mut d) = create_with(
                Codec::H264,
                DecoderOptions {
                    prefer_hw: hw,
                    threads: 2,
                },
                backend,
            ) else {
                println!("{label}: unavailable");
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
            "{label}: ~{:.1} MiB RSS per 1280x720 decoder (8 decoders, after 30 frames each; {})",
            (after.saturating_sub(before)) as f64 / 1024.0 / decoders.len() as f64,
            decoders[0].backend()
        );
    }
}

/// Hardware vs software decoding of 1280x720 streams (600 frames, one key frame):
/// throughput of one stream decoded as fast as possible, and CPU time per frame
/// (all threads of the process, including the download of hardware frames to
/// NV12 / P010 and the copy into `Frame`).
#[test]
#[ignore]
fn hw_vs_sw_720p() {
    let caps = capabilities();
    println!("\n{}", caps.backend);
    for (kind, codec, key) in [
        (Synth::H264, Codec::H264, "h264"),
        (Synth::H265, Codec::H265, "h265"),
        (Synth::H265Main10, Codec::H265, "h265_10"),
        (Synth::Av1Main10, Codec::Av1, "av1_10"),
    ] {
        let Some(stream) = synthetic_720p(kind, 600) else {
            println!("-- {kind:?}: skipped (needs gst-launch-1.0 with an encoder)");
            continue;
        };
        let packets = synthetic_packets(kind, &stream);
        println!(
            "-- {kind:?} 1280x720, {} frames, {} kB",
            packets.iter().filter(|p| !p.config).count(),
            stream.len() / 1000
        );
        let runs: &[(&str, DecoderOptions)] = &[
            (
                "software, 1 thread",
                DecoderOptions {
                    prefer_hw: false,
                    threads: 1,
                },
            ),
            (
                "software, 2 threads",
                DecoderOptions {
                    prefer_hw: false,
                    threads: 2,
                },
            ),
            (
                "hardware",
                DecoderOptions {
                    prefer_hw: true,
                    threads: 2,
                },
            ),
        ];
        for (label, opts) in runs {
            if opts.prefer_hw && !caps.details.iter().any(|d| d.key == key && d.caps.hw) {
                println!("{label:<22} unavailable");
                continue;
            }
            let t0 = Instant::now();
            let Ok(mut dec) = create_with(codec, *opts, Backend::Ffmpeg) else {
                println!("{label:<22} unavailable");
                continue;
            };
            let open_ms = t0.elapsed().as_secs_f64() * 1000.0;
            let (cpu0, start) = (cpu_ns(), Instant::now());
            let mut frames = 0;
            let mut format = None;
            for (i, p) in packets.iter().enumerate() {
                let out = dec.push(&p.data, i as u64, p.key, p.config).unwrap();
                if let Some(f) = out.first() {
                    format = Some(f.format);
                }
                frames += out.len();
            }
            let secs = start.elapsed().as_secs_f64();
            let cpu_ms = match (cpu0, cpu_ns()) {
                (Some(a), Some(b)) => format!("{:.2}", (b - a) as f64 / 1e6 / frames as f64),
                _ => "?".into(),
            };
            println!(
                "{label:<22} {:>6.0} fps  {:>6.2} ms/frame wall  {cpu_ms:>5} ms/frame CPU  open {open_ms:.0} ms  ({}, {:?})",
                frames as f64 / secs,
                secs * 1000.0 / frames as f64,
                dec.backend(),
                format.unwrap_or(PixelFormat::I420)
            );
        }
    }
}
