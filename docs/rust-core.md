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
| `xrmc-cli` | `xrmc` command-line tool to exercise the core without a UI (list, identify, verify, stream to file) | No |

These come next, after the core is proven:

| Crate / app | Responsibility |
|---|---|
| `xrmc-decode` | Hardware decoders: Android `AMediaCodec` → `AHardwareBuffer`, Linux VA-API, Windows Media Foundation/D3D11, macOS VideoToolbox. Software fallback. |
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
