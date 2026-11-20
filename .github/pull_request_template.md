## What and why

<!-- What does this change, and why? Link issues with "Fixes #123". -->

## How it was tested

<!-- Platforms and headsets you tried it on (or "demo mode only"). -->

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] Tried with a real headset: <!-- model, USB / Wi-Fi -->

## Checklist

- [ ] User-visible change noted under `## [Unreleased]` in `CHANGELOG.md`
- [ ] Brand data changed in `src/shared/brands.js` **and** `node crates/xrmc-core/scripts/export-brands.js` run
- [ ] Dependencies changed: `python3 scripts/third-party-notices.py` run, no copyleft-only licences added
- [ ] Docs updated (README / crate READMEs) where needed
