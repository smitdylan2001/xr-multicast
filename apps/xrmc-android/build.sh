#!/usr/bin/env bash
# Builds the XR MultiCast Android app locally:
#   Rust cdylib (cargo-ndk) → jniLibs, scrcpy-server → assets, build/icon.png → launcher icon,
#   then Gradle → APK, copied to dist-native/android/.
#
# Usage: apps/xrmc-android/build.sh [release|debug] [--abi arm64-v8a|x86_64|all] [--fast]
#   release   optimised Rust, APK signed with the local debug key (default)
#   debug     Gradle debug variant (Rust is still built optimised: debug Rust is too slow to decode video)
#   --abi     arm64-v8a for phones/tablets, x86_64 for the emulator; default: both
#   --fast    quicker Rust build for iterating (no LTO, parallel codegen)
#
# Needs: Rust + cargo-ndk, Android SDK/NDK r27 + JDK 17 (env: ANDROID_HOME, ANDROID_NDK_HOME,
# JAVA_HOME; sourced from $XRMC_ANDROID_ENV, default ~/xrmc-spikes/env.sh, when present).
# scrcpy-server v5.0.1: $XRMC_SCRCPY_SERVER, else the repository's vendor/ (or the main
# checkout's vendor/ next to this worktree).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"

ENV_FILE="${XRMC_ANDROID_ENV:-$HOME/xrmc-spikes/env.sh}"
# shellcheck disable=SC1090
[ -f "$ENV_FILE" ] && source "$ENV_FILE"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/xrmc-target-android}"

VARIANT=release
ABIS=(arm64-v8a x86_64)
while [ $# -gt 0 ]; do
  case "$1" in
    release|debug) VARIANT="$1" ;;
    --abi)
      shift
      case "$1" in
        all) ABIS=(arm64-v8a x86_64) ;;
        *) ABIS=("$1") ;;
      esac
      ;;
    --fast)
      export CARGO_PROFILE_RELEASE_LTO=off
      export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
      ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

SCRCPY="${XRMC_SCRCPY_SERVER:-}"
for c in "$ROOT/vendor/scrcpy-server" "$ROOT/../XR View tool/vendor/scrcpy-server"; do
  [ -z "$SCRCPY" ] && [ -f "$c" ] && SCRCPY="$c"
done
if [ -z "$SCRCPY" ] || [ ! -f "$SCRCPY" ]; then
  echo "scrcpy-server not found: set XRMC_SCRCPY_SERVER=/path/to/scrcpy-server (v5.0.1)" >&2
  exit 1
fi

APP="$HERE/android/app/src/main"
TARGETS=()
for a in "${ABIS[@]}"; do TARGETS+=(-t "$a"); done

echo "==> Rust ($VARIANT, ${ABIS[*]}) → $APP/jniLibs"
(cd "$ROOT" && cargo ndk "${TARGETS[@]}" -P 33 --link-libcxx-shared -o "$APP/jniLibs" build --release -p xrmc-android)

echo "==> assets: $SCRCPY, icon"
mkdir -p "$APP/assets" "$APP/res/mipmap-xxxhdpi"
cp "$SCRCPY" "$APP/assets/scrcpy-server"
cp "$ROOT/build/icon.png" "$APP/res/mipmap-xxxhdpi/ic_launcher.png"

echo "==> Gradle assemble${VARIANT^}"
ABI_LIST="$(IFS=,; echo "${ABIS[*]}")"
(cd "$HERE/android" && XRMC_ABIS="$ABI_LIST" ./gradlew --no-daemon -q "assemble${VARIANT^}")

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -n1)"
SUFFIX="$(IFS=-; echo "${ABIS[*]}")"
[ "${#ABIS[@]}" -gt 1 ] && SUFFIX=universal
OUT="$ROOT/dist-native/android"
mkdir -p "$OUT"
APK="$OUT/xr-multicast-$VERSION-android-$SUFFIX-$VARIANT.apk"
cp "$HERE/android/app/build/outputs/apk/$VARIANT/app-$VARIANT.apk" "$APK"
ls -la "$APK"
