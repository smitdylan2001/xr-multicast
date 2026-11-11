# XR MultiCast native core (branch `rust-core`)

A shared Rust core that will power XR MultiCast on **Windows, macOS, Linux and Android** (phones and tablets acting as a hub), and headless Dunk XR hub boxes. iOS is out of scope: it cannot host ADB over USB. Browsers on iPads and other devices remain viewers through the remote web dashboard.

The Electron app on `main` stays the reference implementation until the native apps reach parity. Behaviour is ported with its tests so both stay in sync.

## Why a shared core

- **One implementation** of the ADB client, the scrcpy protocol, device trust and verification, codec and resolution policy, and the remote API. That logic isn't duplicated per platform, and the platforms can't drift apart.
- **Native performance for 10+ headsets:** hardware decoding with zero-copy GPU import, one GPU pass for all tiles, and a low memory baseline. This matters most on Android tablets and small hub boxes.
- **No Google `adb` binary:** the core speaks the ADB protocol itself over USB and TCP, so the app needs no Google software and stays MIT-licensed. It can still use a running adb server to coexist with other tools on desktop.
- **AI-friendly:** a strongly typed, compiled language with fast feedback. The JS tests are ported for parity, and fakes make every layer testable without hardware.

## Crates

| Crate | Responsibility | Platform-specific? |
|---|---|---|
| `xrmc-adb` | ADB wire protocol, RSA auth, USB (`nusb`; Android: a USB file descriptor from `UsbManager`), TCP, adb-server client, multiplexed streams, shell/push/open | Transport only |
| `xrmc-media` | Bitstream parsing (H.264/H.265/AV1 codec strings, bit depth, sizes), fMP4 muxing for browser viewers | No |
| `xrmc-scrcpy` | scrcpy-server v5.0.1 launch arguments, protocol demuxer, encoder list, session over any `AdbDevice` (connects through ADB `OPEN localabstract:`; no port forwarding) | No |
| `xrmc-core` | Brands and model variants (data shared with the Electron app as JSON), layout detection, resolution matching, codec/10-bit policy, settings store compatible with the Electron store file, device-info parsing, LAN/mDNS discovery, device and stream managers with an event channel | No |
| `xrmc-hub` | Wires the core to the real ADB client and scrcpy sessions: `Hub::open()` gives device and stream managers backed by real devices, plus one event channel. Every app builds on this. | No |
| `xrmc-cli` | `xrmc` command-line tool to exercise the core without a UI: `devices`, `connect`, `info`, `encoders`, `stream`, `hub` | No |

These come next, after the core is proven:

| Crate / app | Responsibility |
|---|---|
| `xrmc-decode` | Hardware decoders: Android `AMediaCodec` → `AHardwareBuffer`; FFmpeg hwaccels for Linux (VA-API, V4L2), Windows (D3D11VA/DXVA2) and macOS (VideoToolbox). Software fallback. |
| `xrmc-render` | wgpu compositor (Vulkan / Metal / DX12): all tiles in one pass, the lens-correction shader ported to WGSL, zero-copy import of decoded frames. |
| `xrmc-remote` | WebSocket remote API, wire-compatible with the Electron hub, so the existing tablet web dashboard and hardware tools keep working. |
| `apps/desktop`, `apps/android`, `apps/hubd` | Thin UI/platform shells. The cross-platform UI technology choice is documented in [ui-choice.md](ui-choice.md). |

## Platform targets

| Platform | Notes |
|---|---|
| Linux x86_64 / aarch64 | Desktop and headless hub boxes. USB via `nusb` (udev rule from `packaging/linux`). |
| Windows x86_64 | USB needs a WinUSB-bound ADB interface (Google USB driver / OEM drivers). The adb-server path works everywhere. |
| macOS (Apple Silicon and Intel) | USB via IOKit through `nusb`. |
| Android (modern phones/tablets, Android 13+, Vulkan 1.1+) | Rust library plus a thin Kotlin/Gradle shell for USB permission, mDNS, lifecycle and the surface. Hardware decoding to `AHardwareBuffer`, imported into Vulkan without copies. |

## Building and testing locally

Everything builds and tests on this machine. GitHub CI is manual-only, to save build minutes.

```bash
export PATH=$HOME/.cargo/bin:$PATH
cargo test                                   # all crates
cargo clippy --all-targets -- -D warnings
cargo check --target aarch64-linux-android   # Android
cargo check --target x86_64-pc-windows-gnu   # Windows (compile check)
cargo check --target aarch64-apple-darwin    # macOS (compile check)
cargo run -p xrmc-cli -- --help
```

The Android SDK/NDK/JDK come from the local Unity install (read-only) plus `~/Android/Sdk` for the emulator. See the UI choice document for the exact environment.

## Status (November 2026)

- **Tests:** 285 tests pass across the workspace. Clippy (`-D warnings`) is clean, and `cargo check` passes for Android, Windows and macOS.
- **End to end against a real Android 15 emulator, pure Rust with no Google `adb` involved:**
  - Identify (getprop parsing, panel size, battery, IP) over direct TCP.
  - scrcpy encoder listing.
  - H.264 streaming over direct TCP (connects in ~0.2 s, first frame after ~0.5 s).
  - H.265 streaming through the adb server.
  - Both recordings decode to the correct picture with an independent decoder (GStreamer).
  - `xrmc hub`: the device manager identifies the device and the stream manager casts it (codec policy, quality cap, session lifecycle).
- **Not yet verified on hardware:**
  - Direct USB to real headsets (unit-tested; on desktop the adb server usually owns USB, so the registry falls back to it).
  - The RSA auth prompt on a real device (the emulator has auth disabled).
  - Android 11+ wireless-debugging TLS pairing (not implemented; reported as an error).

### Try it

```bash
export PATH=$HOME/.cargo/bin:$PATH
export XRMC_SCRCPY_SERVER=/path/to/scrcpy-server        # v5.0.1 (npm run setup on main downloads it)
cargo run -p xrmc-cli -- devices
cargo run -p xrmc-cli -- --no-server info 192.168.1.50:5555
cargo run -p xrmc-cli -- stream <serial> --seconds 5 --out cap.h264
cargo run -p xrmc-cli -- hub --seconds 20 --cast <hardware-serial>
```

## Next steps

1. **Android app (egui + Vulkan):**
   - Gradle/GameActivity shell with USB host permission and the file descriptor handed to `AdbRegistry::add_usb_fd`, plus Android mDNS and a multicast lock.
   - Hardware decoding to `AHardwareBuffer`, imported into the wgpu/Vulkan compositor.
   - Verify, add and discover devices, and cast up to 10+ headsets.
2. **`xrmc-render`:** port the lens-correction shader and compositor to WGSL/wgpu.
3. **`xrmc-decode` per platform:** hardware decoding through FFmpeg hwaccels is in.
   - VA-API is verified on Linux: output bit-exact with software, 12 concurrent 720p streams.
   - V4L2 (Raspberry Pi), VideoToolbox and D3D11VA/DXVA2 haven't been tested on real hardware.
   - Next: Android MediaCodec, and zero-copy import into the compositor.
4. **`xrmc-remote`:** remote API server compatible with the existing tablet dashboard and hardware tools.
5. **Desktop app on egui:** reaches parity with the Electron app, then replaces it.
