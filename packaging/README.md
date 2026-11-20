# Packaging

The native Rust app (`apps/xrmc-desktop`, binary `xr-multicast`, and `apps/xrmc-android`) is
the product. The release workflow (`.github/workflows/release.yml`) runs the scripts below on
GitHub's runners; each script also works locally on its own platform. Everything lands in
`dist-native/` (git-ignored). The version comes from the workspace `Cargo.toml`
(`[workspace.package] version`); Android derives `versionName` and `versionCode`
(major·10000 + minor·100 + patch) from it.

The old Electron app (`src/`, `package.json`, `packaging/linux/` except the udev rule) is kept
as source only and is no longer released.

## Packages

`<v>` is the version without the `v`.

| Platform | Files | Script |
|---|---|---|
| Windows 10/11 x64 | `xr-multicast-<v>-windows-x64-setup.exe`, `xr-multicast-<v>-windows-x64.zip` | `native/build-windows.ps1` |
| macOS 11+ (arm64 + x86_64) | `xr-multicast-<v>-macos-universal.dmg` | `native/build-macos.sh` |
| Linux x86_64 (glibc 2.31+) | `xr-multicast-<v>-linux-x86_64.tar.gz`, `xr-multicast_<v>_amd64.deb` | `native/build-linux.sh` |
| Linux aarch64 (glibc 2.36+, Raspberry Pi 5 / SBCs) | `xr-multicast-<v>-linux-aarch64.tar.gz`, `xr-multicast_<v>_arm64.deb` | `native/build-linux.sh` |
| Android 13+ | `xr-multicast-<v>-android-arm64-v8a.apk`, `xr-multicast-<v>-android-universal.apk` | `../apps/xrmc-android/build.sh` |
| All | `SHA256SUMS.txt` | release workflow |

Every package contains `scrcpy-server` v5.0.1 (Apache-2.0; downloaded from the official
scrcpy release and checked against its SHA-256 by `native/fetch-scrcpy-server.sh`), `LICENSE`
and `THIRD_PARTY_NOTICES.md` (when present at the repository root).

## Video decoding per platform

`xrmc-decode` loads FFmpeg at runtime and falls back to OpenH264 (H.264 only); see
`crates/xrmc-decode/README.md`.

| Platform | OpenH264 | FFmpeg |
|---|---|---|
| Windows | compiled in (`--features xrmc-decode/openh264-source`) | bundled: LGPL-3.0 shared DLLs `avcodec-62`, `avutil-60`, `swresample-6` from a pinned BtbN build (FFmpeg 8.1.3) in `ffmpeg\`, with FFmpeg's license and `ffmpeg\README.txt` (source location). Enables D3D11VA/DXVA2 and H.265/AV1. |
| macOS | compiled in | not bundled; Homebrew's FFmpeg (`brew install ffmpeg`) is found automatically (H.265/AV1, VideoToolbox) |
| Linux | always compiled in | the system's libavcodec 59–62 (the .deb recommends it) with VA-API / V4L2 |
| Android | compiled in | not used: MediaCodec |

## Linux: `native/build-linux.sh [x86_64|aarch64|all] [--no-deb]`

Needs rustup, [zig](https://ziglang.org) and `cargo-zigbuild` on `PATH`, and `dpkg-deb` (no
root, no fpm). Both architectures are built with `cargo zigbuild` against an old glibc
(`x86_64-unknown-linux-gnu.2.31`, `aarch64-unknown-linux-gnu.2.36`), so the binaries depend on
nothing but glibc; graphics (Vulkan/EGL), X11/Wayland, FFmpeg and VA-API are loaded at runtime.

- **Tarball:** binaries, scrcpy-server, icons, udev rule, `install.sh` (per-user install,
  `--kiosk`, `--udev`, `--uninstall`).
- **.deb:** `/opt/xr-multicast/` (`xr-multicast`, `xrmc`, `scrcpy-server`), symlinks in
  `/usr/bin`, desktop entry and icons, the USB rule `/lib/udev/rules.d/70-xr-multicast-adb.rules`
  (reloaded in `postinst`). Depends on `libc6`, `libxkbcommon0`, `libvulkan1 | libegl1`;
  recommends `libavcodec62 | … | libavcodec59`, Mesa drivers and the X11/Wayland client
  libraries. It replaces the Electron 1.x package of the same name.

## Windows: `pwsh packaging/native/build-windows.ps1 [-SkipInstaller]`

Needs Visual Studio Build Tools (MSVC, Windows SDK), rustup and Inno Setup 6 (installed with
Chocolatey when missing). Builds `x86_64-pc-windows-msvc` with a static CRT (no VC++
redistributable needed) and the icon/version resource (`apps/xrmc-desktop/build.rs`,
`windows/xr-multicast.ico`). Release builds are GUI programs (no console window).

- **Zip:** `xr-multicast-<v>-windows-x64\` with `xr-multicast.exe`, `xrmc.exe`,
  `scrcpy-server`, `LICENSE.txt`, `THIRD_PARTY_NOTICES.md`, `ffmpeg\`.
- **Installer** (`windows/xr-multicast.iss`): per-user install by default (no admin), with an
  all-users option; Start-menu shortcut, optional desktop shortcut, uninstaller.

To update FFmpeg, change the URL, SHA-256 and DLL names at the top of the script (keep a
month-end `autobuild-YYYY-MM-*` release, those are kept long-term, and libavcodec 59–62).

## macOS: `packaging/native/build-macos.sh`

Needs Xcode or the Command Line Tools and rustup. Builds both Apple targets
(`MACOSX_DEPLOYMENT_TARGET=11.0`), `lipo`s them into `XR MultiCast.app`
(`Contents/MacOS/xr-multicast` + `xrmc`, `Contents/Resources/scrcpy-server`, `AppIcon.icns`
made from `build/icon.png` with `sips`/`iconutil`, `Info.plist` with the local-network usage
text), signs ad hoc (or with `MACOS_SIGN_IDENTITY`) and makes a compressed dmg with an
`Applications` link. It isn't notarized, so Gatekeeper blocks the first start: right-click →
Open, or `xattr -dr com.apple.quarantine "/Applications/XR MultiCast.app"`.

## Android: `apps/xrmc-android/build.sh release --abi all`

See `apps/xrmc-android/README.md`. With several ABIs it builds ABI splits plus a universal APK
and copies `…-android-universal.apk` and `…-android-arm64-v8a.apk`. Release signing uses
`XRMC_KEYSTORE`, `XRMC_KEYSTORE_PASSWORD`, `XRMC_KEY_ALIAS`, `XRMC_KEY_PASSWORD`; without them the
debug key signs. In CI the keystore comes from repository secrets (see `docs/releasing.md`).

## Releasing

See [`docs/releasing.md`](../docs/releasing.md).
