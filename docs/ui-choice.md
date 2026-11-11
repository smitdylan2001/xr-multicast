# Cross-platform UI: egui/eframe + wgpu

**Decision:** the native apps (Windows, macOS, Linux, Android) use **egui/eframe** for the UI and **wgpu** for rendering, with the stream compositor as a custom render pass inside the same GPU device.

## Requirements

- One codebase for Windows, macOS, Linux and Android (Android 13+, Vulkan 1.1+).
- 10+ live video tiles in one GPU pass with a custom lens-correction shader, plus a path to zero-copy import of hardware-decoded frames.
- A license compatible with an MIT project **and** with Dunk XR's commercial embedded hardware, with no per-device fees and no GPL.
- Mostly AI-written code: one language, strong typing, lots of public examples, fast local builds.
- Lighter than Electron.

## Comparison

| Candidate | Android | Many tiles + our shader | Zero-copy decode | License (MIT app + embedded hardware) | Verdict |
|---|---|---|---|---|---|
| **egui/eframe + wgpu** | Official support | Proven in a spike: one pass, 16 tiles @ 60 fps | wgpu raw-device interop; small raw-Vulkan step on Android | MIT/Apache, no restrictions | **Chosen** |
| Slint | Yes | Good (wgpu textures) | Via wgpu | GPLv3, or a royalty-free license that excludes embedded, or paid per device | License blocks hardware products |
| iced | Not official | Good | Pinned to an older wgpu | MIT | No Android |
| Dioxus / Blitz | Webview / experimental | Webview only | Immature | MIT/Apache | Not ready |
| Tauri | Yes | Webview only | Weak on Linux (WebKitGTK) | MIT/Apache | Same limits as today |
| Flutter + flutter_rust_bridge | Excellent | One platform texture per stream | Separate glue per platform | BSD | Three languages, uneven video interop |
| Compose Multiplatform + UniFFI | Excellent | Poor on desktop | Android only | Apache/MPL | JVM on desktop (heavier than Electron) |
| Qt/QML + cxx-qt | Mature | Excellent | Via Qt Multimedia | LGPLv3 duties; some modules GPL; embedded per-device fees | Licensing and toolchain weight |
| Makepad / Freya / Xilem | Niche / new / alpha | Own engines | Hard | MIT/Apache | Too immature |

## Spike evidence

Code and scripts are in `~/xrmc-spikes/`, measured on a Radeon 780M.

| Platform | Result |
|---|---|
| Linux (Wayland) | 9 tiles @ 60 fps with 1.1 GB/s of texture uploads: 15–24 % of one core, ~120–133 MiB (baseline ~87–100 MiB). 16 tiles @ 60 fps: ~150 MiB. |
| Android emulator (API 35, Vulkan 1.3) | 9 tiles @ 60 fps, ~80 MB of app memory, cold start 392 ms, 29 MB APK. The app's own Vulkan device has the Android frame-import extensions and YCbCr sampling enabled. Soft-keyboard typing, tap/double-tap and pause/resume work. |
| Windows / macOS | `cargo check` passes, including the interop path. Linking was not possible on the Linux build machine; build and test natively later. |

## Versions

- Rust 1.99 (edition 2024).
- egui/eframe 0.37, wgpu 30, winit 0.30.
- android-activity 0.6 (GameActivity, AAR `games-activity:4.4.0`).
- NDK r27c; AGP 9.5 / Gradle 9.9; minSdk 33, targetSdk 36.

## Known risks and how we handle them

| Risk | Plan |
|---|---|
| Not native-looking; responsive layout is manual | Build our own theme and widget layer early, with large touch targets on Android. |
| Android soft keyboard | winit drops GameActivity text events. A ~60-line bridge fixes this. Still to do: number/IP keyboard types and lifting content above the keyboard. |
| Accessibility on Android not yet validated | egui uses AccessKit. Test with TalkBack before release. |
| egui changes its API every few months | Pin versions; upgrade deliberately. |
| wgpu has no YCbCr samplers | A small raw-Vulkan pass converts vendor-format decoder frames on Android. |
| A hidden window gets no frames on Wayland | No core logic on the UI thread. The core runs on tokio and the UI only renders state. |

## Bonus

egui compiles to WebAssembly, so the browser dashboard for tablets could later become the same Rust UI.
