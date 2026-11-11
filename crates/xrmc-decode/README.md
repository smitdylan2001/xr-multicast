# xrmc-decode

Turns scrcpy video packets (H.264 / H.265 Annex B, AV1 OBUs) into CPU YUV frames
(`I420`, `Nv12`, `I420P10`) for `xrmc-render`.

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
   (FFmpeg 5.x–8.x); other majors are refused.
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
(FFmpeg warnings to stderr; it's silent by default), `XRMC_OPENH264_LIB`.

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

Only the leading fields of `AVPacket` and `AVFrame` are read or written. Everything on `AVCodecContext` goes through AVOptions. There are three `AVFrame` layouts, selected by the libavutil major (see `src/ffmpeg/sys.rs`).

How the layouts were verified:

- **Headers:** checked with `offsetof()` against the official headers of FFmpeg 5.1, 6.0, 6.1, 7.0, 7.1, 8.0 and 8.1. Run `scripts/check-ffmpeg-layouts.sh` to repeat this; the `layouts_match_ffmpeg_headers` unit test pins the offsets.
- **Full test suite, run against real libraries:**
  - FFmpeg 8.1.2 (the system copy)
  - FFmpeg 7.1.1 (libavcodec 61)
  - FFmpeg 6.1.1 (libavcodec 60, from a PyAV wheel)

  Point `XRMC_FFMPEG_DIR` and `LD_LIBRARY_PATH` at the folder to run it against another copy.

## Hardware decoding (TODO)

`hw` is reported as `false` everywhere. VA-API, V4L2, VideoToolbox and D3D11VA through FFmpeg need these fields on `AVCodecContext`:

- `hw_device_ctx`
- the `get_format` callback

Their offsets differ between majors, so they need the same per-major verification as `AVFrame`. They also need `av_hwdevice_ctx_create` and `av_hwframe_transfer_data` to download NV12/P010. Android (MediaCodec) and zero-copy import are planned as separate backends.

## Tests

`cargo test -p xrmc-decode` runs offline:

- **Test streams:** synthetic SMPTE-bar streams in `tests/data/`, plus real scrcpy recordings from an Android emulator. See `tests/data/generate.sh`.
- **Packetising:** the tests split each stream into scrcpy-style packets.
- **Skipping:** FFmpeg tests are skipped with a message if no supported libavcodec is found.

Benchmark and memory (ignored by default):

```bash
cargo test -p xrmc-decode --release --test bench -- --ignored --nocapture --test-threads=1
```
