#!/usr/bin/env bash
# Build the native XR MultiCast app for macOS as a universal (arm64 + x86_64) app bundle in a dmg.
#
#   packaging/native/build-macos.sh
#
# Runs on macOS with Xcode (or the Command Line Tools) and rustup. OpenH264 is compiled in
# (H.264 always works); FFmpeg is not bundled: a Homebrew FFmpeg (`brew install ffmpeg`) is
# picked up automatically for H.265/AV1 and VideoToolbox hardware decoding.
# The app is ad-hoc signed (not notarized): first start with right-click → Open, or
# `xattr -dr com.apple.quarantine "/Applications/XR MultiCast.app"`.
# Set MACOS_SIGN_IDENTITY to sign with a Developer ID instead.
# Output: dist-native/xr-multicast-<version>-macos-universal.dmg
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | cut -d'"' -f2)"
OUT="$ROOT/dist-native"
JAR="${XRMC_SCRCPY_SERVER:-}"
[[ -z "$JAR" ]] && JAR="$("$ROOT/packaging/native/fetch-scrcpy-server.sh")"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-11.0}"
IDENTITY="${MACOS_SIGN_IDENTITY:--}"

TARGETS=(aarch64-apple-darwin x86_64-apple-darwin)
rustup target add "${TARGETS[@]}" >/dev/null
for t in "${TARGETS[@]}"; do
  echo "==> building $t"
  (cd "$ROOT" && cargo build --locked --release -p xrmc-desktop -p xrmc-cli --target "$t" \
    --features xrmc-decode/openh264-source)
done

WORK="$OUT/macos"
APP="$WORK/XR MultiCast.app"
rm -rf "$WORK"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
for b in xr-multicast xrmc; do
  lipo -create -output "$APP/Contents/MacOS/$b" \
    "$CARGO_TARGET_DIR/aarch64-apple-darwin/release/$b" "$CARGO_TARGET_DIR/x86_64-apple-darwin/release/$b"
done
lipo -info "$APP/Contents/MacOS/xr-multicast"
cp "$JAR" "$APP/Contents/Resources/scrcpy-server"
cp "$ROOT/LICENSE" "$APP/Contents/Resources/"
[[ -f "$ROOT/THIRD_PARTY_NOTICES.md" ]] && cp "$ROOT/THIRD_PARTY_NOTICES.md" "$APP/Contents/Resources/"

echo "==> icon"
ICONSET="$WORK/AppIcon.iconset"
mkdir -p "$ICONSET"
for s in 16 32 128 256 512; do
  sips -z "$s" "$s" "$ROOT/build/icon.png" --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
  d=$((s * 2))
  if [[ $d -le 512 ]]; then
    sips -z "$d" "$d" "$ROOT/build/icon.png" --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
  fi
done
cp "$ROOT/build/icon.png" "$ICONSET/icon_512x512@2x.png" # 512 px source: best available
iconutil -c icns -o "$APP/Contents/Resources/AppIcon.icns" "$ICONSET"
rm -rf "$ICONSET"

cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleDisplayName</key><string>XR MultiCast</string>
  <key>CFBundleName</key><string>XR MultiCast</string>
  <key>CFBundleExecutable</key><string>xr-multicast</string>
  <key>CFBundleIdentifier</key><string>com.dunkxr.xrmulticast</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>$MACOSX_DEPLOYMENT_TARGET</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSHumanReadableCopyright</key><string>MIT License · Dunk XR / Dunk Technologies</string>
  <key>NSLocalNetworkUsageDescription</key>
  <string>XR MultiCast connects to VR headsets on your network (wireless ADB) to show their screens.</string>
  <key>NSBonjourServices</key>
  <array>
    <string>_adb-tls-connect._tcp</string>
    <string>_adb._tcp</string>
  </array>
</dict>
</plist>
EOF
plutil -lint "$APP/Contents/Info.plist"

echo "==> signing ($IDENTITY)"
codesign --force --sign "$IDENTITY" --timestamp=none "$APP/Contents/MacOS/xrmc"
codesign --force --sign "$IDENTITY" --timestamp=none "$APP"
codesign --verify --strict --verbose=2 "$APP"

echo "==> dmg"
DMG_ROOT="$WORK/dmg"
mkdir -p "$DMG_ROOT"
cp -R "$APP" "$DMG_ROOT/"
ln -s /Applications "$DMG_ROOT/Applications"
DMG="$OUT/xr-multicast-$VERSION-macos-universal.dmg"
rm -f "$DMG"
hdiutil create -volname "XR MultiCast $VERSION" -srcfolder "$DMG_ROOT" -fs HFS+ -format UDZO -ov "$DMG" >/dev/null
rm -rf "$DMG_ROOT"
echo "    $DMG ($(du -h "$DMG" | cut -f1))"
