#!/usr/bin/env bash
# Build the native XR MultiCast app for Linux and package it as a tarball.
#
#   packaging/native/build-linux.sh [x86_64|aarch64|all]
#
# aarch64 (Raspberry Pi 5 and other single-board computers) is cross-compiled
# with cargo-zigbuild against glibc 2.36 (Debian 12 / Raspberry Pi OS Bookworm
# and newer). Needs: rustup targets, `zig` and `cargo-zigbuild` on PATH.
# Output: dist-native/xr-multicast-<version>-linux-<arch>.tar.gz
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ARCHES="${1:-all}"
[[ "$ARCHES" == "all" ]] && ARCHES="x86_64 aarch64"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | cut -d'"' -f2)"
OUT="$ROOT/dist-native"
JAR="${XRMC_SCRCPY_SERVER:-}"
if [[ -z "$JAR" ]]; then
  # The Electron checkout (main branch) downloads it with `npm run setup`.
  for c in "$ROOT/vendor/scrcpy-server" "$ROOT/../XR View tool/vendor/scrcpy-server"; do
    [[ -f "$c" ]] && JAR="$c" && break
  done
fi
if [[ -z "$JAR" || ! -f "$JAR" ]]; then
  echo "scrcpy-server v5.0.1 not found: set XRMC_SCRCPY_SERVER or run 'npm run setup' on the main branch" >&2
  exit 1
fi
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/xrmc-target-dist}"
mkdir -p "$OUT"

for arch in $ARCHES; do
  case "$arch" in
    x86_64) target=x86_64-unknown-linux-gnu; build=(cargo build) ;;
    aarch64) target=aarch64-unknown-linux-gnu; build=(cargo zigbuild) ;;
    *) echo "unknown arch $arch" >&2; exit 1 ;;
  esac
  zig_target="$target"
  [[ "$arch" == "aarch64" ]] && zig_target="$target.2.36"
  echo "==> building $arch"
  (cd "$ROOT" && "${build[@]}" --release -p xrmc-desktop -p xrmc-cli --target "$zig_target")
  bin="$CARGO_TARGET_DIR/$target/release"
  stage="$OUT/xr-multicast-$VERSION-linux-$arch"
  rm -rf "$stage"
  mkdir -p "$stage"
  cp "$bin/xr-multicast" "$bin/xrmc" "$stage/"
  cp "$JAR" "$stage/scrcpy-server"
  cp "$ROOT/build/icon.png" "$ROOT/build/icon.svg" "$stage/"
  cp "$ROOT/packaging/linux/70-xr-multicast-adb.rules" "$stage/"
  cp "$ROOT/packaging/native/install.sh" "$stage/"
  cp "$ROOT/LICENSE" "$stage/"
  tar -C "$OUT" -czf "$stage.tar.gz" "$(basename "$stage")"
  echo "    $stage.tar.gz ($(du -h "$stage.tar.gz" | cut -f1))"
done
