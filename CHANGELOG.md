# Changelog

All notable changes to XR MultiCast are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.0.0] - 2026-11-20

The first native release. XR MultiCast is rewritten in Rust as one app for
Windows, macOS, Linux (x86_64 and aarch64) and Android. The Electron app stays in
the repository as legacy source but is no longer released.

### Added

- **Native app** built with egui/eframe and wgpu: touch-first UI, sidebar drawer
  on narrow and portrait screens, adjustable interface size, fullscreen (`F11`),
  focus (`1`–`9`, double-tap), grid layouts, PNG snapshots and toasts.
- **Built-in ADB client** (`xrmc-adb`): ADB over USB (`nusb`) and TCP with RSA
  authentication, written in Rust. Google's `adb` is no longer needed. A running
  adb server is still used when present, and its key is tried too, so headsets
  that already trust the computer don't prompt again.
- **Android standalone hub**: phones and tablets (Android 13+, Vulkan 1.1) host
  headsets over USB-C (OTG) or Wi-Fi, decode with MediaCodec, and keep casting in
  the background through a foreground service.
- **Raspberry Pi and single-board computers**: aarch64 Linux builds and a kiosk
  mode (`--kiosk`: fullscreen, no window decorations, confirmed quit) with
  autostart from `install.sh --kiosk`.
- **Hardware video decoding** through FFmpeg hwaccels loaded at runtime (VA-API,
  V4L2 request and M2M, D3D11VA/DXVA2, VideoToolbox) with a per-stream software
  fallback, plus a built-in OpenH264 decoder for H.264.
- **Video codecs view**: the headset's hardware encoders next to this receiver's
  decoders (H.264 / H.265 / AV1, 8 and 10 bit), with the negotiated codec and why.
- **Lens-correction compositor** in WGSL: every tile in one GPU pass, 10-bit
  (P010) textures where available.
- **Tilt calibration** for canted eye panels, mirrored per eye; Meta Quest 3
  preset with a 25° tilt.
- **"Waiting for video"**: a stream only shows LIVE once a picture arrives;
  sessions that send no video (display off, authorisation prompt pending) are
  restarted without dropping to a lower codec.
- Command-line flags: `--demo N`, `--fullscreen`, `--kiosk`, `--ui-scale`,
  `--connect`, `--renderer auto|vulkan|gl`, `--no-adb-server`, `--no-usb`,
  `--store`, `--adb-key`, `--scrcpy-server`, `--size`, `--decoder-threads`.
- `xrmc` command-line tool for headless testing (`devices`, `info`, `encoders`,
  `stream`, `hub`).
- Packages: Windows installer and portable zip (with LGPL FFmpeg), macOS
  universal `.dmg`, Linux `.deb` and tarballs for x86_64 and aarch64, Android
  APKs, and `SHA256SUMS.txt`.
- `THIRD_PARTY_NOTICES.md`, `CONTRIBUTING.md`, `SECURITY.md`, `CODE_OF_CONDUCT.md`
  and issue templates.

### Changed

- Settings and verified headsets are read from the same `xr-multicast.json` as
  1.x, so existing setups carry over.
- Brand data is exported from `src/shared/brands.js` to
  `crates/xrmc-core/data/brands.json` and shared by both apps.
- The core logic (device trust, discovery, codec and resolution policy, layout
  detection) is ported from the Electron app together with its tests.

### Removed

- The Electron app is no longer released. Its source stays in `src/` and still
  provides the tablet/browser dashboard and the remote WebSocket API until those
  are ported.
- The on-demand download of Google's Platform-Tools (not needed any more).

### Known limitations

- Not yet verified on real hardware: Raspberry Pi, native Windows and macOS runs,
  and an Android device hosting a real headset over USB.
- No remote dashboard or remote API in the native app yet; no Android 11+
  wireless-debugging pairing; builds are not code-signed.

## [1.0.0] - 2026-11-11

First public release: an Electron app for Windows, macOS and Linux.

### Added

- Multi-headset casting over USB or Wi-Fi with scrcpy-server v5.0.1, for Meta
  Quest, PICO, Play For Dream / YVR, HTC VIVE and Android XR headsets.
- Lens correction (Corrected, Both eyes, Raw), panel layout auto-detection and a
  per-headset calibration panel.
- USB-first verification, one-click Wi-Fi, network scan (saved addresses, mDNS,
  subnet scan) and auto-reconnect.
- H.264, H.265 and AV1 with automatic codec choice and fallback; experimental
  10-bit; quality presets per transport; streaming one eye only.
- Stream resolution matched to the tile size, and one shared GPU surface for all
  streams.
- Tablet and phone dashboard in the browser with PIN pairing, and a remote
  WebSocket API for external tools.
- Colour-coded status bar with explanations and fixes.
- Optional download of adb after accepting Google's terms.

[Unreleased]: https://github.com/smitdylan2001/xr-multicast/compare/v2.0.0...HEAD
[2.0.0]: https://github.com/smitdylan2001/xr-multicast/releases/tag/v2.0.0
[1.0.0]: https://github.com/smitdylan2001/xr-multicast/tree/14b8bbf
