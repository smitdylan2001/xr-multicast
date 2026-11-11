# xrmc-core

Platform-independent core of XR MultiCast: a port of the Electron app's main
process (the reference implementation), shared by the desktop apps, Android
and headless hubs.

| Module | Ported from | What |
| --- | --- | --- |
| `brands` | `src/shared/brands.js` | Headset brands, model variants, lens presets, detection |
| `layout` | `src/renderer/js/layout-detect.js` | Eye-view grid detection, layout/rotation resolution |
| `view_size` | `src/shared/view-size.js` | Stream resolution matched to the tile size |
| `codec_policy` | `src/main/stream-manager.js` | Codec / 10-bit / quality choice, decoder-support merging |
| `settings`, `store` | `src/main/config.js`, `src/main/store.js` | Settings and the `xr-multicast.json` store (same file format) |
| `device_info` | `src/main/device-manager.js`, `src/main/adb.js` | adb shell commands and output parsers |
| `discovery` | `src/main/discovery.js` | Subnet scan and mDNS (`_adb-tls-connect._tcp`, `_adb._tcp`) |
| `device_manager` | `src/main/device-manager.js` | Polling, identify, USB verification, Wi-Fi, auto-reconnect |
| `stream_manager` | `src/main/stream-manager.js` | Stream state machine, retries, codec fallbacks, resizing |

ADB goes through `xrmc_adb::AdbDevice` (+ `device_manager::AdbBackend`), scrcpy
sessions through `stream_manager::StreamBackend`, so everything is testable
with fakes. Events (`events::CoreEvent`) go out on one `tokio::sync::broadcast`
channel.

## Brand data

`data/brands.json` is generated from the Electron app, which stays the single
source of truth. After changing `src/shared/brands.js`, run:

```sh
node crates/xrmc-core/scripts/export-brands.js
```

`cargo test` fails while the embedded file is stale (the check is skipped when
`node` isn't available; set `NODE=/path/to/node` to point at one).

## Parity with the JS tests

`layout-detect`, `view-size`, `codec-choice` and `stream-resize` tests are
ported case by case (same names, same expectations). Keep both in step.
