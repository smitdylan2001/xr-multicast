# XR MultiCast native app (`xr-multicast`)

Casts many standalone VR headsets into one lens-corrected, clearly labelled dashboard.
Runs on Windows, macOS and Linux, including single-board computers (Raspberry Pi 5 class)
with a touchscreen acting as the hub. Built with egui/eframe and wgpu, on top of
`xrmc-hub` (devices and streams), `xrmc-decode` (video decoding) and `xrmc-render`
(lens-correction compositor).

## Run

```bash
cargo run --release -p xrmc-desktop                      # real headsets
cargo run --release -p xrmc-desktop -- --demo 9          # 9 synthetic headsets
cargo run --release -p xrmc-desktop -- --kiosk           # touchscreen hub
```

| Flag | Meaning |
|---|---|
| `--demo N` | N synthetic headsets that cast through the real stream manager and decode workers. Nothing touches devices or the settings file. |
| `--fullscreen` | Start fullscreen (F11 or the toolbar button toggle it). |
| `--kiosk` | Fullscreen without window decorations; quitting needs a confirmation (Settings → Quit…). |
| `--ui-scale X` | Interface size on top of the display scaling (also in Settings → Display). |
| `--no-adb-server` | Don't use a running adb server; talk to headsets over USB and TCP directly. |
| `--no-usb` | Don't open USB devices directly. |
| `--connect ADDR` | Connect to `ip[:port]` on start (repeatable). |
| `--renderer auto\|vulkan\|gl` | GPU backend (auto prefers Vulkan/Metal/DX12 and falls back to OpenGL). |
| `--store PATH`, `--adb-key PATH`, `--scrcpy-server PATH` | Override the default files. |
| `--size WxH`, `--decoder-threads N` | Window size; decoder threads per stream. |

Settings and verified headsets are shared with the Electron app (`xr-multicast.json`
in `~/.config/XR MultiCast/`, `%APPDATA%\XR MultiCast\`, `~/Library/Application Support/XR MultiCast/`).
scrcpy-server is looked up next to the executable, then `$XRMC_SCRCPY_SERVER`, then the repository's `vendor/`.

## Architecture

| Module | Role |
|---|---|
| `bridge` | A tokio runtime (2 worker threads) owns the hub. The UI sends `Action`s and receives `UiEvent`s; it never blocks. The event loop routes video packets into per-stream bounded queues, assigns a decoder generation per session, and handles decode failures (10-bit → 8-bit → H.264 fallback). |
| `decode` | One thread per stream: decodes with `xrmc-decode`, keeps only the newest frame, wakes the UI, and runs layout detection on a 96×96 luma grid (500 ms until found, then 3 s, then 30 s). On queue overflow it skips to the next key frame. |
| `model` | The view model and pure logic: headset cards, tile status, tile render parameters (brand/model preset + saved calibration + overrides → `TileParams`), stats line, grid layout, view-size requests. |
| `view_report` | Debounced tile-size reports (`set_view_sizes("local", …)`) so each stream's resolution matches its tile. |
| `ui/*` | Top bar, sidebar (drawer on narrow/portrait screens), tiles, dialogs, calibration drawer (bottom sheet on portrait), status bar, toasts, vector icons. |
| `demo` | Synthetic headsets: a `StreamBackend` and `DeviceDirectory` for the core's `StreamManager`, and a decoder that draws stereo test panels. |
| `offscreen` | Renders the whole app into a texture at any size (no window): screenshots for any screen from any machine. |

## Testing

```bash
cargo test -p xrmc-desktop
cargo clippy -p xrmc-desktop --all-targets -- -D warnings
# Screenshots without a window (hidden flags):
xr-multicast --demo 9 --offscreen 800x1280 --screenshot portrait.png --exit-after 6
xr-multicast --demo 9 --offscreen 1600x1000 --show calibrate --screenshot calib.png --exit-after 5
# …or of the real window (uses egui's viewport screenshot):
xr-multicast --demo 9 --screenshot window.png --exit-after 8
```

`--show` opens `settings`, `verify`, `edit`, `calibrate`, `guide`, `add-ip`, `focus`, `nav`, `fix` or `snapshot`
after 2 seconds. `--cast-on-start` casts every castable headset once (unattended tests).
