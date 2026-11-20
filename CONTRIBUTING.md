# Contributing to XR MultiCast

Thanks for helping! Bug reports, headset reports, calibration values, docs and code
are all welcome. Please follow the [Code of Conduct](CODE_OF_CONDUCT.md).

- **Bugs:** open a [bug report](https://github.com/smitdylan2001/xr-multicast/issues/new?template=bug_report.yml).
- **A headset that doesn't work well, or one that does:** use the [headset support](https://github.com/smitdylan2001/xr-multicast/issues/new?template=headset_support.yml) template.
- **Ideas:** open a [feature request](https://github.com/smitdylan2001/xr-multicast/issues/new?template=feature_request.yml).
- **Security issues:** don't open an issue; see [SECURITY.md](SECURITY.md).

## Development setup

1. Install Rust with [rustup](https://rustup.rs). The toolchain and targets are pinned in
   [`rust-toolchain.toml`](rust-toolchain.toml) and installed automatically.
2. Get scrcpy-server v5.0.1 into `vendor/scrcpy-server`: download `scrcpy-server-v5.0.1` from
   the [scrcpy releases](https://github.com/Genymobile/scrcpy/releases/tag/v5.0.1), or run
   `npm install && npm run setup` (needs Node.js 18+). You can also point
   `XRMC_SCRCPY_SERVER` at the file.
3. Run the app:

   ```bash
   cargo run -p xrmc-desktop --release                # real headsets
   cargo run -p xrmc-desktop --release -- --demo 9    # synthetic headsets, no hardware
   cargo run -p xrmc-cli -- --help                    # the core without a UI
   ```

Optional:

- **FFmpeg** (libavcodec 59–62) for H.265/AV1 and hardware decoding. Without it, H.264 uses
  OpenH264. Set `XRMC_FFMPEG_DIR` to try another build.
- **Android:** cargo-ndk, Android SDK 36, NDK r27 and JDK 17; see
  [apps/xrmc-android/README.md](apps/xrmc-android/README.md).
- **Node.js** for the legacy Electron app (`npm install && npm start`, `npm test`) and for
  exporting brand data.

## Before you open a pull request

Run the checks locally:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

If you touch platform-specific code, also run a compile check for the other targets, e.g.
`cargo check --target x86_64-pc-windows-gnu`, `--target aarch64-apple-darwin` or
`--target aarch64-linux-android`.

**Please run tests locally rather than relying on CI.** GitHub Actions only runs when started
manually (and on release tags) to save build minutes. Mention in the PR what you ran and on
which hardware.

Tests must not need a headset: the core uses fakes for ADB and scrcpy, and decoder tests use
recorded streams in `crates/xrmc-decode/tests/data/`. Hardware tests skip themselves when the
hardware is missing.

## Sharing calibration presets

Lens presets are starting points per brand and model. If you've calibrated a headset so that
straight lines look straight, please share the values:

1. In the app, open the tile's menu → **Calibrate**, turn on the reference grid, and adjust
   until lines in the middle *and* near the edges are straight. Save.
2. Note the values shown: k1, k2, zoom, lens centre X/Y, tilt, red/blue fringe, and the panel
   layout and rotation if you changed them. They're also saved per headset in
   `xr-multicast.json` (see the README for its location).
3. Either open a [headset support](https://github.com/smitdylan2001/xr-multicast/issues/new?template=headset_support.yml)
   issue with the values, the headset model and its software version, and ideally a *Raw*
   and a *Corrected* snapshot (camera button on the tile); or send a pull request:
   - Edit [`src/shared/brands.js`](src/shared/brands.js), the single source of truth for brand
     data. Change the brand's `correction`, or add a `variants` entry matching the model (see
     the Quest 3 and Play For Dream MR variants).
   - Regenerate the Rust copy: `node crates/xrmc-core/scripts/export-brands.js` (this writes
     `crates/xrmc-core/data/brands.json`; `cargo test` fails while it is stale).
   - Run `cargo test -p xrmc-core` and `npm test`.

## Code style

- Rust 2024 edition, formatted with `rustfmt`, clippy-clean with `-D warnings`.
- Keep platform-specific code behind the `Platform` trait (`apps/xrmc-desktop/src/platform.rs`)
  or `cfg` blocks in the crate that needs it. Core crates stay platform-independent.
- Comments explain *why*, not what. Match the style of the code around you.
- No new dependencies with copyleft licences (GPL/LGPL/AGPL/MPL as the only option): the app
  must stay usable in MIT and commercial hardware products. After changing dependencies, run
  `python3 scripts/third-party-notices.py` and commit the updated `THIRD_PARTY_NOTICES.md`.
- When the Electron app and the Rust core share behaviour, keep their tests in step (see
  [crates/xrmc-core/README.md](crates/xrmc-core/README.md)).

## Commits and pull requests

- One logical change per commit. Write the subject in the imperative or as a short summary,
  optionally prefixed with the crate or area: `xrmc-decode: retry hardware decoder after …`,
  `docs: …`, `Android: …`.
- Explain in the body what changed and why, and how you tested it (especially on real
  headsets).
- Add a line under `## [Unreleased]` in [CHANGELOG.md](CHANGELOG.md) for user-visible changes.
- Fill in the pull request template.

By contributing, you agree that your contributions are licensed under the [MIT License](LICENSE).
