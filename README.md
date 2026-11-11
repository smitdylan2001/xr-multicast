# XR MultiCast

**Cast every standalone VR headset in the room to one screen: lens-corrected, clearly labelled, over USB or Wi-Fi.**

[![CI](https://github.com/smitdylan2001/xr-multicast/actions/workflows/ci.yml/badge.svg)](https://github.com/smitdylan2001/xr-multicast/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

XR MultiCast is a desktop app for **Windows, macOS and Linux** that mirrors standalone headsets into one dashboard. It is built for classrooms, demo booths, training sessions and labs: anywhere one person needs to see what several headset users are looking at. Tablets and phones can join the dashboard from a browser.

Instead of the raw fisheye panel image, you get a **flat, natural "vision-corrected" view** of what the wearer sees. Each stream is labelled with the headset's number, name and colour.

XR MultiCast is built and maintained by **[Dunk XR](https://dunktechnologies.com)** (Dunk Technologies), which also offers **whitelabel versions and custom hardware solutions** built on it. See [Whitelabel & custom hardware](#whitelabel--custom-hardware-by-dunk-xr).

| Brand | Recognised models |
| --- | --- |
| **Meta** | Quest, Quest 2, Quest Pro, Quest 3, Quest 3S |
| **PICO** | PICO 4, PICO 4 Ultra, Neo 3 |
| **Play For Dream / YVR** | YVR 1/2, Play For Dream MR |
| **HTC VIVE** | VIVE Focus 3, XR Elite, Focus Vision |
| **Android XR** | Samsung Galaxy XR, and any device that reports `android.software.xr.*` |

> **Project status: early (v1).** The core pipeline (connection, streaming, decoding, lens correction, codecs, tablet access) is implemented and covered by automated tests. It has been verified end to end with synthetic streams. Real-world testing across headset models is ongoing, and lens-correction presets are starting points you can calibrate per headset. Reports and calibration values for your headsets are very welcome. See [Contributing](#contributing).

---

## Contents

- [Features](#features)
- [How it works](#how-it-works)
- [Getting started](#getting-started)
- [Connecting a headset](#connecting-a-headset)
- [Tablets & phones](#tablets--phones)
- [Building your own tools on top](#building-your-own-tools-on-top)
- [Whitelabel & custom hardware by Dunk XR](#whitelabel--custom-hardware-by-dunk-xr)
- [Installing on Linux](#installing-on-linux)
- [Building from source](#building-from-source)
- [Troubleshooting](#troubleshooting)
- [Project layout](#project-layout)
- [Contributing](#contributing)
- [License](#license)

## Features

**Connecting**
- **USB-first verification.** A headset is plugged in once over USB and verified by its hardware serial before it may cast. Unverified devices found on the network are listed but can't cast. This is on by default and can be turned off in Settings.
- **One-click Wi-Fi.** Verifying can switch the headset to network ADB, so you can unplug the cable. Verified headsets reconnect over Wi-Fi automatically.
- **Network discovery.** *Scan network* finds headsets by saved address, by mDNS, and by scanning the local subnet for the ADB port.
- **Cast all.** One click streams every verified headset. Dropped streams retry, and switch to Wi-Fi if the cable is pulled.

**Viewing**
- **Clear labels.** Each headset gets a number, name and colour, shown in the sidebar, the stream header and as an overlay on the video, including in fullscreen. Each stream also shows a brand badge, model, connection, battery, resolution, fps and bitrate.
- **Vision correction:**
  - **Corrected** (default): one eye, lens distortion removed.
  - **Both eyes**: stereo pair, corrected.
  - **Raw**: the unprocessed fisheye panel image.
- **Panel layout auto-detection.** The app works out from the video whether the panel is side by side, stacked or a 2×2 grid (e.g. Play For Dream MR), and shows a single upright left-eye view by default.
- **Calibrate** per headset with a live reference grid:
  - Barrel distortion (k1/k2), zoom and lens centre.
  - Colour-fringe correction.
  - Layout and rotation.
  - Experimental: flips and eye-pair selection.

  Calibration is saved per headset.
- **Rotate** button on every stream, focus mode (double-click or keys `1`–`9`), fullscreen, PNG snapshots and grid layouts.

**Video**
- **Codecs:** Auto, H.264, H.265 (HEVC) or AV1. *Auto* checks which hardware encoders the headset has and what the viewing devices can decode, and falls back to H.264 automatically on any failure.
- **Bandwidth:** presets or custom resolution, bitrate (0.5–200 Mbps) and fps, set separately for USB and Wi-Fi.
- **Stream one eye only:** the headset crops before encoding, giving about twice the sharpness at the same bandwidth.
- **10-bit colour** (experimental): H.265 Main 10 or 10-bit AV1, with automatic fallback to 8-bit.

**Sharing**
- **Tablets & phones** (iPad, Android, iPhone) open the dashboard in a browser, pair once with a PIN and can be added to the home screen.
- **Remote API.** The same WebSocket API is open to your own software and hardware. See [Building your own tools on top](#building-your-own-tools-on-top).

## How it works

All supported headsets run Android, so XR MultiCast uses the same building blocks for every brand:

```
 Headset (Android)                         Hub computer (XR MultiCast)                 Viewers
┌──────────────────────┐   USB / Wi-Fi   ┌──────────────────────────────────────┐   ┌─────────────────┐
│ scrcpy-server        │      (adb)      │ adb + scrcpy protocol demuxer        │   │ Desktop window  │
│ captures the display │ ──────────────► │ WebCodecs decode (GPU)               │──►│ (Electron)      │
│ → H.264/H.265/AV1    │                 │ WebGL lens correction + layout detect│   │                 │
│   hardware encoder   │                 │ LAN web server + WebSocket (opt.)    │──►│ Tablets/phones  │
└──────────────────────┘                 └──────────────────────────────────────┘   │ (browser, MSE)  │
                                                                                    └─────────────────┘
```

- **ADB** connects to each headset (USB, `adb tcpip` Wi-Fi, or mDNS).
- **[scrcpy-server](https://github.com/Genymobile/scrcpy)** (pinned v5.0.1) is pushed to the headset and encodes its display with the headset's hardware encoder. XR MultiCast implements the scrcpy v5 client protocol itself.
- The desktop decodes with **WebCodecs**, using the GPU where available.
- A **WebGL shader** undoes the lens pre-distortion. For each output pixel at radius *r* from the lens axis it samples the panel at `r_src = r · (1 + k1·r² + k2·r⁴) · s_channel`, with `s_channel` correcting red and blue fringing.
- Tablets on plain `http://` have no WebCodecs (it needs a secure context), so the browser **remuxes H.264 to fragmented MP4** and plays it through Media Source Extensions, then applies the same shader.

## Getting started

**Download** an installer from [Releases](https://github.com/smitdylan2001/xr-multicast/releases): `.exe` for Windows, `.dmg` for macOS, `.deb`/`.AppImage` for Linux. Or run from source:

```bash
git clone https://github.com/smitdylan2001/xr-multicast.git
cd xr-multicast
npm install
npm run setup     # downloads scrcpy-server v5.0.1 into vendor/
npm start
```

Running from source requires Node.js 18 or newer.

### adb (Android Platform-Tools)

XR MultiCast talks to headsets through Google's **adb**. To keep the project purely MIT-licensed, **adb is not included** in this repository or in the installers.

- If you already have adb, the app finds it automatically. That covers an Android SDK or Android Studio install, `apt install adb`, `brew install android-platform-tools` and `winget install Google.PlatformTools`.
- If not, the app shows an **Install adb** dialog on first launch. It explains the licence, links to the [Android SDK License Agreement](https://developer.android.com/studio/terms), and only downloads after you tick that you agree. The official package comes straight from `dl.google.com` into the app's own data folder, so nothing is installed system-wide. You can update it later from **Settings → About**.
- Developers can fetch it for local builds with `npm run setup -- --with-adb`. Doing so also means you accept Google's terms.
- `XRMC_ADB=/path/to/adb` forces a specific adb.

## Connecting a headset

1. **Enable developer mode / USB debugging** on the headset. The in-app *How to connect* guide has steps for each brand. Meta Quest needs Developer Mode in the Meta Horizon phone app (a free Meta developer account).
2. **Plug it in** with a USB-C *data* cable. Put the headset on and accept **Allow USB debugging**, ticking *Always allow*.
3. Click **Verify**, give the headset a label and colour, and keep *Enable Wi-Fi casting* ticked.
4. Click **Cast** or **Cast all**.

**Platform notes**
- **Windows:** Meta Quest needs the *Oculus ADB Drivers*; other brands use the Google USB Driver or the vendor driver.
- **macOS:** no driver needed.
- **Linux:** see [Installing on Linux](#installing-on-linux) for USB permissions.

## Tablets & phones

The computer stays the hub: it keeps the USB/ADB connections and forwards the video.

1. On the hub: **Settings → Remote access**, tick *Let tablets and phones…*, **Save**. The default port is 8787; allow it through your firewall.
2. On the tablet, scan the QR code or open the address shown (e.g. `http://192.168.1.20:8787`). Enter the 6-digit **PIN** once.
3. Optionally, *Add to Home Screen* to use it like an app.

- *Allow remote control* off means view-only. Verifying/forgetting headsets and all settings are hub-only, always.
- Over plain `http://`, tablets play **H.264 only**. While one is connected, streams use a codec every viewer can play.
- Supported browsers: iPadOS 13+ Safari, iOS 17.1+ Safari, and current Chrome / Edge / Samsung Internet on Android. Each viewer only receives the streams it is showing.

## Building your own tools on top

XR MultiCast is meant to be a foundation. The hub exposes a **WebSocket API**, the same one the tablet UI uses, so you can build:
- **Hardware controllers:** a "cast all" button, a Stream Deck or rotary selector to focus a headset, status LEDs per headset (battery, live, offline).
- **Kiosks and wall displays:** a Raspberry Pi showing selected streams.
- **Automation:** start casts when a session begins, log battery levels, integrate with lab or event software.

Pair your device once with the hub's PIN, then call methods such as `stream:startAll`, `stream:start`, `devices:scan`, and listen to `devices` / `state` events. Control-only devices declare `support: false` so they never affect video codecs.

**→ Full protocol, permissions and a Node.js example: [docs/remote-api.md](docs/remote-api.md)**

Inside the app, the main extension points are:
- `src/shared/brands.js`: headset detection, model variants, lens presets.
- `src/main/stream-manager.js`: stream lifecycle, codec policy.
- `src/main/main.js`: the handler registry; add a method there and it's available over IPC and the remote API, with a permission level.

## Whitelabel & custom hardware by Dunk XR

XR MultiCast is developed by **[Dunk XR](https://dunktechnologies.com)**, the immersive-technology studio of **Dunk Technologies**. Dunk XR has over seven years of hands-on VR and AR development experience and provides:

- **Immersive development:** custom VR, AR and MR applications for training, education, healthcare, events and entertainment.
- **Workshops:** hands-on sessions from VR basics to advanced AR techniques, tailored to your team.
- **Consultancy:** expert guidance to get immersive projects unstuck and shipped.

**Built on XR MultiCast:**

- **Whitelabel versions:** XR MultiCast under your own brand, with your name, logo and colours, preconfigured for your headsets and venues, and installers you can distribute to your customers.
- **Custom hardware solutions:** turnkey multi-headset casting stations. Examples include dedicated hub computers, wall-display and kiosk setups, physical control panels (cast-all buttons, headset selectors, status lights) and charging/storage carts with integrated casting.
- **Integrations and custom features:** connecting XR MultiCast to your LMS, session software or venue systems through the [remote API](docs/remote-api.md), or adding features for your use case.

Get in touch: **[info@dunktechnologies.com](mailto:info@dunktechnologies.com)** · [dunktechnologies.com](https://dunktechnologies.com) · [LinkedIn](https://www.linkedin.com/in/smitdylan/)

## Installing on Linux

**Per-user install** (no root needed):

```bash
npm run install:linux                              # build + install to ~/.local/opt/xr-multicast, app-grid entry, `xr-multicast` command
bash packaging/linux/install-user.sh --system      # recommended, once (sudo): AppArmor profile + USB rule
bash packaging/linux/install-user.sh --uninstall   # remove
```

**System package:** `npm run dist:linux`, then `sudo apt install ./dist/xr-multicast_1.0.0_amd64.deb`. This installs to `/opt/XR MultiCast` and includes the AppArmor profile and USB rule. Use one method, not both.

- **AppArmor (Ubuntu 24.04+):**
  - **Problem:** Ubuntu blocks the user namespaces Chromium's sandbox needs unless the app has an AppArmor profile. Without one, Electron apps exit at launch with *"The SUID sandbox helper binary was found, but is not configured correctly"*.
  - **Until the profile is installed:** the per-user launcher starts the app with `--no-sandbox` and the status bar shows **Chromium sandbox off**.
  - **Fix:** `--system` (or the .deb) installs the profile.
- **USB permissions:** `android-udev-rules` only lists specific product IDs. Quest 3S/Pro, newer PICO, Play For Dream / YVR and newer VIVE may show *no permissions*. `70-xr-multicast-adb.rules` matches the standard ADB USB interface instead, so every brand works.

## Building from source

```bash
npm test             # unit tests (protocol parsing, codecs, layout detection, remote server, fMP4 muxer)
npm run dist:win     # NSIS installer + portable .exe
npm run dist:mac     # .dmg
npm run dist:linux   # .deb + AppImage
```

Installers don't contain adb (see [adb](#adb-android-platform-tools)). Pushing a tag like `v1.0.0` builds all three platforms on GitHub Actions and attaches the installers to a release.

The scrcpy client/server protocol is internal and changes between scrcpy versions. The server version is pinned in `src/main/config.js`. If you bump it, re-run `npm run setup` and check `src/main/stream-session.js` against that release's `doc/develop.md`.

## Troubleshooting

The status bar is colour-coded: **green** means fine, **amber** means it works but with a caveat, and **red** means something is broken. Every amber or red item, including the low-battery indicator and the adb banner, can be clicked for an explanation and the fix.

- **Wi-Fi casting stops after a headset reboots.** Network ADB resets on reboot. Plug in via USB once and click *Enable Wi-Fi*.
- **Scan finds nothing.** The hub and headsets must share a subnet. Guest networks and client isolation block this.
- **adb not installed.** Click it in the status bar to install it, or install adb yourself. See [adb](#adb-android-platform-tools).
- **"adb server version doesn't match".** Another tool (SideQuest, MQDH, Android Studio) runs a different adb. Close it, or point XR MultiCast at that adb with `XRMC_ADB=/path/to/adb`.
- **Black video.** The headset display must be awake to be mirrored.
- **Sideways or upside-down image.** Use the ↻ button on the stream (saved per headset). Please report the correct angle for your model so the preset can be fixed.

## Project layout

```
src/main/                Electron main process
  main.js                  handler registry (IPC + remote API), app lifecycle
  adb.js                   adb wrapper and discovery ($XRMC_ADB → app-installed → existing installs)
  adb-installer.js         downloads Platform-Tools from Google after the user accepts the terms
  device-manager.js        discovery, identification, USB verification, Wi-Fi, auto-reconnect
  discovery.js             LAN subnet scan for the ADB port
  stream-session.js        scrcpy-server launch + scrcpy v5 protocol demuxer, encoder listing
  stream-manager.js        stream lifecycle, retries, codec/10-bit negotiation, quality presets
  remote-server.js         LAN web server + WebSocket (PIN pairing, GOP cache, backpressure)
  store.js                 verified headsets + settings (JSON in the user data folder)
src/shared/brands.js     brand/model detection, labels, setup steps, lens presets
src/preload.js           IPC bridge (context isolation, sandboxed renderer)
src/renderer/            UI (no framework)
  js/decoder.js            WebCodecs decoder (H.264/H.265/AV1, bit-depth detection)
  js/lens-renderer.js      WebGL lens correction, layouts, rotation/flip, frame analysis
  js/layout-detect.js      panel layout detection from luminance profiles
  js/mse-decoder.js        H.264 → fragmented MP4 → Media Source Extensions (tablets)
  js/remote-bridge.js      window.xr over WebSocket (browser clients)
packaging/linux/         per-user installer, .deb scripts, udev rule
docs/remote-api.md       WebSocket API for external tools
```

## Contributing

Issues and pull requests are welcome. Especially useful:

- **Headset reports:** model, what works, and a *Raw* snapshot (camera button on a stream) so layouts and rotation presets can be verified.
- **Calibration values** that make lines straight on your model. They're in the Calibrate panel and can be added to `src/shared/brands.js`.
- Testing on Windows, macOS, and physical tablets.

Run `npm test` before opening a PR. The code style is plain JavaScript, 2-space indent, single quotes, no frameworks.

## License

XR MultiCast is released under the [MIT License](LICENSE). © 2026 Dylan Smit (Dunk Technologies).

**Third-party components**
- **[scrcpy-server](https://github.com/Genymobile/scrcpy)** (Apache License 2.0, © Genymobile and Romain Vimont) is downloaded by `npm run setup` and bundled in release builds. Apache 2.0 allows this alongside MIT code.
- **Android SDK Platform-Tools** (`adb`) are **not** part of this repository or the installers. When needed, the app downloads them from Google on the user's request, after the user accepts the [Android SDK License Agreement](https://developer.android.com/studio/terms).
- npm dependencies (`ws`, `qrcode`, `extract-zip`, Electron) are MIT-licensed or similar; see their packages.
