#!/usr/bin/env bash
# Builds the XR MultiCast Android app locally:
#   Rust cdylib (cargo-ndk) → jniLibs, scrcpy-server → assets, build/icon.png → launcher icon,
#   then Gradle → APK, copied to dist-native/android/.
#
# Usage: apps/xrmc-android/build.sh [release|debug] [--abi arm64-v8a|x86_64|all] [--fast]
#   release   optimised Rust, release APK (default)
#   debug     Gradle debug variant (Rust is still built optimised: debug Rust is too slow to decode video)
#   --abi     arm64-v8a for phones/tablets, x86_64 for the emulator; default: all (both)
#   --fast    quicker Rust build for iterating (no LTO, parallel codegen)
#
# Output (dist-native/android/), <v> = workspace version from Cargo.toml:
#   release, all ABIs:  xr-multicast-<v>-android-universal.apk and xr-multicast-<v>-android-arm64-v8a.apk
#   release, one ABI:   xr-multicast-<v>-android-<abi>.apk
#   debug:              the same names with a -debug suffix
#
# Signing (release): set XRMC_KEYSTORE=/path/to/release.jks, XRMC_KEYSTORE_PASSWORD, XRMC_KEY_ALIAS
# and XRMC_KEY_PASSWORD (defaults to the store password) to sign with your key; otherwise the
# APK is signed with the local debug key (~/.android/debug.keystore).
#
# Needs: Rust + cargo-ndk, Android SDK/NDK r27 + JDK 17 (env: ANDROID_HOME, ANDROID_NDK_HOME,
# JAVA_HOME; sourced from $XRMC_ANDROID_ENV, default ~/xrmc-spikes/env.sh, when present).
# scrcpy-server v5.0.1: $XRMC_SCRCPY_SERVER, else the repository's vendor/ (or the main
# checkout's vendor/ next to this worktree), else downloaded by packaging/native/fetch-scrcpy-server.sh.
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
# Otherwise download it (checksum-verified) into vendor/.
[ -z "$SCRCPY" ] && SCRCPY="$("$ROOT/packaging/native/fetch-scrcpy-server.sh")"
if [ ! -f "$SCRCPY" ]; then
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

if [ -n "${XRMC_KEYSTORE:-}" ]; then
  [ -f "$XRMC_KEYSTORE" ] || { echo "XRMC_KEYSTORE not found: $XRMC_KEYSTORE" >&2; exit 1; }
  XRMC_KEYSTORE="$(cd "$(dirname "$XRMC_KEYSTORE")" && pwd)/$(basename "$XRMC_KEYSTORE")"
  export XRMC_KEYSTORE
  SIGNING="release key ($XRMC_KEYSTORE)"
else
  SIGNING="debug key"
fi

SPLITS=0
[ "${#ABIS[@]}" -gt 1 ] && SPLITS=1
echo "==> Gradle assemble${VARIANT^} (signed with the $SIGNING)"
ABI_LIST="$(IFS=,; echo "${ABIS[*]}")"
rm -rf "$HERE/android/app/build/outputs/apk/$VARIANT"
(cd "$HERE/android" && XRMC_ABIS="$ABI_LIST" XRMC_SPLITS="$SPLITS" ./gradlew --no-daemon -q "assemble${VARIANT^}")

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -n1)"
TAG=""
[ "$VARIANT" = debug ] && TAG="-debug"
OUT="$ROOT/dist-native/android"
mkdir -p "$OUT"
APKS="$HERE/android/app/build/outputs/apk/$VARIANT"
if [ "$SPLITS" = 1 ]; then
  cp "$APKS/app-universal-$VARIANT.apk" "$OUT/xr-multicast-$VERSION-android-universal$TAG.apk"
  for a in "${ABIS[@]}"; do
    # The x86_64 APK is for the emulator only; the universal APK covers it.
    [ "$a" = x86_64 ] && continue
    cp "$APKS/app-$a-$VARIANT.apk" "$OUT/xr-multicast-$VERSION-android-$a$TAG.apk"
  done
else
  cp "$APKS/app-$VARIANT.apk" "$OUT/xr-multicast-$VERSION-android-${ABIS[0]}$TAG.apk"
fi
ls -la "$OUT"/xr-multicast-"$VERSION"-android-*"$TAG".apk
