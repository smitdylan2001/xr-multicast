# xrmc-decode

Turns scrcpy video packets (H.264 / H.265 Annex B, AV1 OBUs) into CPU YUV frames
(`I420`, `Nv12`, `I420P10`, `P010`) for `xrmc-render`, in hardware where the
machine can.

```rust
let caps = xrmc_decode::capabilities();          // what this machine can decode
let mut dec = xrmc_decode::create(Codec::H264, DecoderOptions::default())?;
for frame in dec.push(&packet, pts_us, is_key, is_config)? { /* upload */ }
```

## Backends

`create()` tries them in this order (`create_with()` forces one):

1. **FFmpeg, loaded at runtime** with `libloading` (never linked or bundled by the
   build). H.264 (`h264`), H.265 (`hevc`, 8 and 10 bit), AV1 (`libdav1d`, else
   `libaom-av1`, 8 and 10 bit). Supported: libavcodec 59–62 / libavutil 57–60
   (FFmpeg 5.x–8.x); other majors are refused. Hardware decoding through FFmpeg's
   hwaccels when available (see below).
   Search order, newest major first in each place:
   `$XRMC_FFMPEG_DIR`, `<exe dir>/ffmpeg`, (macOS: `<exe>/../Frameworks`,
   `<exe>/../lib`), `<exe dir>`, (macOS: Homebrew/MacPorts dirs), then the system
   library path (`libavcodec.so.62`, `libavcodec.62.dylib`, `avcodec-62.dll`, …),
   then unversioned names (Android).
2. **OpenH264** (H.264 only), via the `openh264` crate. On Linux (x86_64 and
   aarch64 boards) it is compiled from source, so it is always available. Other
   targets compile it with `--features xrmc-decode/openh264-source` (needs that
   target's C++ toolchain, e.g. MSVC, Xcode, the NDK), or load a Cisco OpenH264
   binary at runtime (`$XRMC_OPENH264_LIB`, or `openh264*.dll` / `libopenh264*`
   next to the executable). It's off by default there so that
   `cargo check --target …` works from Linux without cross compilers.

Environment: `XRMC_FFMPEG_DIR`, `XRMC_FFMPEG_DISABLE=1`, `XRMC_FFMPEG_LOG=1`
(FFmpeg warnings to stderr; it's silent by default), `XRMC_OPENH264_LIB`,
`XRMC_HWDEC`, `XRMC_HWDEC_DEVICE` (see below).

## Hardware decoding

`DecoderOptions::prefer_hw` (the default) opens the decoder on a hardware path
when the probe found one for that codec. Frames are downloaded to system memory
(`Nv12`, or `P010` for 10-bit), so `is_hardware()` decoders produce the same
`Frame`s as software ones. `backend()` reads e.g. `ffmpeg:hevc+vaapi`.

| Platform | Tried automatically, in order |
|---|---|
| Linux | VA-API on every `/dev/dri/renderD*` node; the `drm` device for V4L2 request hwaccels (Raspberry Pi 5 HEVC in Raspberry Pi's FFmpeg); the stateful `h264_v4l2m2m` / `hevc_v4l2m2m` decoders (Raspberry Pi 4 and other SBCs; only when a V4L2 device's name looks like a decoder, so cameras aren't opened) |
| macOS | VideoToolbox |
| Windows | D3D11VA, then DXVA2 |
| Android | none here (the MediaCodec backend handles Android) |

AV1 uses FFmpeg's native `av1` decoder, which only works through a hwaccel. In
software it stays `libdav1d`.

- **Probe:** on first use (`capabilities()` or the first `create()`), each
  profile (`h264`, `h265`, `h265_10`, `av1`, `av1_10`) decodes a tiny embedded
  128×128 SMPTE-bar key frame (`src/ffmpeg/probe/`, ~13 KB) through the hardware
  path. It only counts if the picture comes out right. The probe runs on its own
  thread with a 3 s timeout, so a hanging driver can't block the app (it takes
  ~0.15 s on a Radeon 780M). `Capabilities.details[].hw_api` names the device,
  e.g. `VA-API (AMD Radeon 780M Graphics)`, taken from the VA driver's vendor
  string.
- **One device per decoder:** each stream opens its own device (VADisplay).
  Mesa's VA-API driver holds one lock per display while it waits for the GPU,
  so twelve 720p streams on one display reach 567 fps in total, against 1220 fps
  with twelve displays.
- **Fallback to software, per stream, logged once:** the stream is reopened in
  software (keeping its config, with a failed key frame decoded again at once)
  if any of these happen:
  - the hwaccel doesn't accept the stream (`get_format` isn't offered the device
    format, e.g. H.264 High 10);
  - the hardware path fails before it produced a picture;
  - three key frames in a row fail;
  - `av_hwframe_transfer_data` fails;
  - a frame has an implausible size or format.

  An ordinary corrupt packet doesn't count: the decoder stays in hardware and
  waits for the next key frame.
- **Mislabelled 10-bit HEVC:** streams that signal the Main (8-bit) profile but
  carry 10-bit samples are never handed to the hwaccel. FFmpeg's VA-API hwaccel
  then sets up 8-bit surfaces, and on AMD (VCN 4, Mesa 26) the decoder writes past
  them. That causes a GPU page fault and a ring or full GPU reset, and Mesa aborts
  the process. GStreamer's `vah265enc` (1.26) produces such streams.
- **Low latency:** a hwaccel decoder still outputs each frame from the `push` of
  its own packet. It runs with one thread, and the download blocks until that
  frame is decoded.
- **Environment:**
  - `XRMC_HWDEC=0` disables hardware decoding.
  - `XRMC_HWDEC=vaapi|v4l2|v4l2request|drm|v4l2m2m|videotoolbox|d3d11va|dxva2|vulkan|cuda|…`
    tries only that device type (`v4l2` = `drm` + `v4l2m2m`; any FFmpeg device
    type name works).
  - `XRMC_HWDEC_DEVICE` sets the device, e.g. `/dev/dri/renderD129`.

## Stream handling

This works like the Electron `StreamDecoder` (`src/renderer/js/decoder.js`):

- Config packets are remembered:
  - **H.264/H.265:** SPS/PPS/VPS are prepended to every key frame.
  - **AV1:** the `av1C` `configOBUs`, or raw sequence-header OBUs, are inserted after the key frame's temporal delimiter, unless the key frame already has a sequence header.
- Delta frames are dropped until the first key frame, after a config change, and after a decoder error. `Decoder::waiting_for_key_frame()` lets the caller request a key frame.
- After an error, the FFmpeg decoder is flushed. OpenH264 is recreated, because it otherwise rejects the next key frame.
- **Low latency:**
  - **FFmpeg:** `flags=+low_delay` (this also disables frame threading), `flags2=+fast`, `thread_type=slice`, and libdav1d `max_frame_delay=1`. Settings go through AVOptions only.
  - **OpenH264:** decodes without delay, and flushes when a decode call returns no picture.
  - Each packet's frame comes out of the same `push`.
- **Colour:**
  - **FFmpeg:** the frame's `colorspace`/`color_range`.
  - **OpenH264:** the SPS VUI.
  - If unspecified, BT.709 for HD (shorter side ≥ 720) and BT.601 below, limited range unless the stream is `yuvj` or full range is signalled.

## FFmpeg ABI notes

Only the leading fields of `AVPacket` and `AVFrame` are read or written. There are three `AVFrame` layouts, selected by the libavutil major (see `src/ffmpeg/sys.rs`).

`AVCodecContext` settings go through AVOptions, with these exceptions for hardware decoding:

- **Written:** `opaque`, `get_format`, `hw_device_ctx`.
- **Read in `get_format`:** `profile`, `sw_pix_fmt`.

These offsets differ between majors (59: 48/152/872, 60: 48/152/864, 61 and 62: 48/192/560 for the written fields). Other safeguards:

- Hardware decoding is refused on unverified majors and on 32-bit targets.
- A fresh context is checked for the expected defaults before anything is written.
- `AVCodecHWConfig` is identical in every supported release.

How the layouts were verified:

- **Headers:** checked with `offsetof()` against the official headers of FFmpeg 5.1, 6.0, 6.1, 7.0, 7.1, 8.0 and 8.1.
  - x86_64 Linux: compiled and run.
  - x86_64 Windows (LLP64), aarch64 macOS and aarch64 Linux: cross-compiled with `zig cc`; all identical.
  - Run `scripts/check-ffmpeg-layouts.sh` to repeat this. The `layouts_match_ffmpeg_headers` and `codec_context_offsets_match_ffmpeg_headers` unit tests pin the offsets.
  - At runtime, `get_format_reads_profile_and_sw_pix_fmt` checks the values read against the loaded library.
- **Full test suite, run against real libraries:**
  - FFmpeg 8.1.2 (the system copy, with VA-API)
  - FFmpeg 7.1.5 (libavcodec 61, a BtbN LGPL shared build, with VA-API)
  - FFmpeg 7.1.1 (libavcodec 61, from a Unity package; no hwaccels)
  - FFmpeg 6.1.1 (libavcodec 60, Ubuntu's build bundled by another app, with VA-API; needs the system libva first on `LD_LIBRARY_PATH`)
  - FFmpeg 6.1.1 (libavcodec 60, from a PyAV wheel; no hwaccels)
  - FFmpeg 5.1 (libavcodec 59) is checked against its headers only.

  Point `XRMC_FFMPEG_DIR` and `LD_LIBRARY_PATH` at the folder to run it against another copy.

## Tests

`cargo test -p xrmc-decode` runs offline:

- **Test streams:** synthetic SMPTE-bar streams in `tests/data/`, plus real scrcpy recordings from an Android emulator. See `tests/data/generate.sh`.
- **Packetising:** the tests split each stream into scrcpy-style packets.
- **Skipping:** FFmpeg tests are skipped with a message if no supported libavcodec is found.
- **Hardware (`tests/hw.rs`):** these skip with a message when there's no hardware decoder.
  - Every test stream is decoded in hardware and compared with software. On VA-API (Radeon 780M) all 10 streams, H.264, HEVC 8/10-bit and AV1 8/10-bit, are bit-exact.
  - Key-frame waiting and recovery.
  - The fallback for an unsupported profile (H.264 High 10).
  - 12 concurrent 1280×720 hardware decoders.
  - Environment overrides and broken devices (`XRMC_HWDEC=0`/`bogus`, `/dev/null` or a missing render node), each in a child process.

Benchmark and memory (ignored by default):

```bash
cargo test -p xrmc-decode --release --test bench -- --ignored --nocapture --test-threads=1
```

`hw_vs_sw_720p` results on a Ryzen 7 8845HS / Radeon 780M (FFmpeg 8.1, Mesa 26.2). Each figure is one 1280×720 stream decoded flat out, and CPU time covers all threads.

| Codec | Software | Hardware |
|---|---|---|
| H.264 | 290 fps, 3.0 ms CPU/frame | 140–180 fps, 0.8–2.1 ms CPU/frame |
| HEVC | 220 fps (2 threads), 4.4 ms | 390 fps, 0.8 ms |
| HEVC Main 10 | 380 fps, 4.3 ms | 320 fps, 1.5 ms |
| AV1 10-bit (libdav1d in software) | 420 fps, 2.7 ms | 190 fps, 1.8–3.6 ms |

- Most of the hardware CPU time is the download: `vaGetImage` plus a copy, about 0.7 ms for NV12 and 1.0 ms for P010 per 720p frame.
- Hardware wall time per frame (about 3 ms) is mostly waiting for the GPU.
- Twelve concurrent hardware streams reach about 1200 fps in total, so 12 × 60 fps fits.
- A hardware decoder adds about 0.2 MiB RSS; its surfaces are in GPU memory.
