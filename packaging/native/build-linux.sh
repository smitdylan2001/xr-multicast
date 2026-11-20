#!/usr/bin/env bash
# Build the native XR MultiCast app for Linux and package it as a tarball and a Debian package.
#
#   packaging/native/build-linux.sh [x86_64|aarch64|all] [--no-deb]
#
# Both architectures are built with cargo-zigbuild against an old glibc so one binary runs on
# many distributions:
#   x86_64   glibc 2.31 (Ubuntu 20.04+, Debian 11+)
#   aarch64  glibc 2.36 (Raspberry Pi OS / Debian 12 Bookworm+, Ubuntu 23.04+; Raspberry Pi 5 and other SBCs)
# Needs: rustup targets, `zig` and `cargo-zigbuild` on PATH, `dpkg-deb` for the .deb (any distro
# with dpkg; no root needed).
# Output (dist-native/):
#   xr-multicast-<version>-linux-<arch>.tar.gz   (unpack and run, or ./install.sh)
#   xr-multicast_<version>_<amd64|arm64>.deb
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ARCHES=all
DEB=1
for a in "$@"; do
  case "$a" in
    --no-deb) DEB=0 ;;
    x86_64|aarch64|all) ARCHES="$a" ;;
    *) echo "unknown argument $a" >&2; exit 2 ;;
  esac
done
[[ "$ARCHES" == "all" ]] && ARCHES="x86_64 aarch64"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | cut -d'"' -f2)"
OUT="$ROOT/dist-native"
JAR="${XRMC_SCRCPY_SERVER:-}"
[[ -z "$JAR" ]] && JAR="$("$ROOT/packaging/native/fetch-scrcpy-server.sh")"
[[ -f "$JAR" ]] || { echo "scrcpy-server not found: $JAR" >&2; exit 1; }
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/xrmc-target-dist}"
mkdir -p "$OUT"

# Extra files shipped in every package (THIRD_PARTY_NOTICES.md only if present).
docs=("$ROOT/LICENSE")
[[ -f "$ROOT/THIRD_PARTY_NOTICES.md" ]] && docs+=("$ROOT/THIRD_PARTY_NOTICES.md")

make_deb() {
  local arch="$1" stage="$2" glibc="$3" deb_arch root pkg
  case "$arch" in x86_64) deb_arch=amd64 ;; aarch64) deb_arch=arm64 ;; esac
  root="$(mktemp -d)"
  pkg="$OUT/xr-multicast_${VERSION}_${deb_arch}.deb"
  install -d "$root/DEBIAN" "$root/opt/xr-multicast" "$root/usr/bin" "$root/usr/share/applications" \
    "$root/usr/share/icons/hicolor/512x512/apps" "$root/usr/share/icons/hicolor/scalable/apps" \
    "$root/lib/udev/rules.d" "$root/usr/share/doc/xr-multicast"
  install -m 0755 "$stage/xr-multicast" "$stage/xrmc" "$root/opt/xr-multicast/"
  install -m 0644 "$stage/scrcpy-server" "$root/opt/xr-multicast/"
  ln -s /opt/xr-multicast/xr-multicast "$root/usr/bin/xr-multicast"
  ln -s /opt/xr-multicast/xrmc "$root/usr/bin/xrmc"
  install -m 0644 "$ROOT/build/icon.png" "$root/usr/share/icons/hicolor/512x512/apps/xr-multicast.png"
  install -m 0644 "$ROOT/build/icon.svg" "$root/usr/share/icons/hicolor/scalable/apps/xr-multicast.svg"
  install -m 0644 "$ROOT/packaging/linux/70-xr-multicast-adb.rules" "$root/lib/udev/rules.d/"
  install -m 0644 "$ROOT/LICENSE" "$root/usr/share/doc/xr-multicast/copyright"
  for f in "${docs[@]}"; do install -m 0644 "$f" "$root/usr/share/doc/xr-multicast/"; done
  cat > "$root/usr/share/applications/xr-multicast.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=XR MultiCast
GenericName=VR Headset Casting
Comment=Cast and monitor VR headset screens over USB or Wi-Fi
Exec=/opt/xr-multicast/xr-multicast
Icon=xr-multicast
Terminal=false
Categories=Utility;
Keywords=VR;XR;Quest;Meta;Pico;Vive;headset;cast;mirror;
StartupNotify=true
StartupWMClass=xr-multicast
EOF
  cat > "$root/DEBIAN/control" <<EOF
Package: xr-multicast
Version: $VERSION
Architecture: $deb_arch
Maintainer: Dunk XR / Dunk Technologies <https://github.com/smitdylan2001/xr-multicast>
Installed-Size: $(du -sk --exclude=DEBIAN "$root" | cut -f1)
Depends: libc6 (>= $glibc), libxkbcommon0, libvulkan1 | libegl1
Recommends: libavcodec62 | libavcodec61 | libavcodec60 | libavcodec59, mesa-vulkan-drivers | libegl-mesa0, libegl1, libwayland-client0 | libx11-6, libwayland-egl1, libx11-xcb1, libxkbcommon-x11-0, libxcursor1, libxi6, libva-drm2
Section: video
Priority: optional
Homepage: https://github.com/smitdylan2001/xr-multicast
Description: Cast and monitor many standalone VR headsets on one screen
 XR MultiCast shows the screens of many standalone VR headsets (Meta Quest,
 PICO, VIVE and other Android-based headsets) in one lens-corrected, clearly
 labelled dashboard, over USB or Wi-Fi. It talks ADB itself and runs
 scrcpy-server (Apache-2.0, bundled) on each headset.
 .
 Video decoding uses FFmpeg (libavcodec 59-62) when installed, with hardware
 decoding through VA-API or V4L2, and the built-in OpenH264 decoder otherwise.
EOF
  cat > "$root/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = configure ]; then
  # The Electron 1.x package managed /usr/bin/xr-multicast with update-alternatives and copied
  # the USB rule to /etc; tidy up after it.
  [ -e /usr/bin/xr-multicast ] || ln -sf /opt/xr-multicast/xr-multicast /usr/bin/xr-multicast
  if [ -f /etc/udev/rules.d/70-xr-multicast-adb.rules ] &&
    cmp -s /etc/udev/rules.d/70-xr-multicast-adb.rules /lib/udev/rules.d/70-xr-multicast-adb.rules; then
    rm -f /etc/udev/rules.d/70-xr-multicast-adb.rules
  fi
  if command -v udevadm >/dev/null 2>&1; then
    udevadm control --reload-rules || true
    udevadm trigger --subsystem-match=usb --action=change || true
  fi
fi
exit 0
EOF
  cat > "$root/DEBIAN/postrm" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = remove ] && command -v udevadm >/dev/null 2>&1; then
  udevadm control --reload-rules || true
fi
exit 0
EOF
  chmod 0755 "$root/DEBIAN/postinst" "$root/DEBIAN/postrm"
  chmod -R u+rwX,go-w,go+rX "$root"
  dpkg-deb --root-owner-group -Zxz --build "$root" "$pkg" >/dev/null
  rm -rf "$root"
  echo "    $pkg ($(du -h "$pkg" | cut -f1))"
}

for arch in $ARCHES; do
  case "$arch" in
    x86_64) target=x86_64-unknown-linux-gnu; glibc=2.31 ;;
    aarch64) target=aarch64-unknown-linux-gnu; glibc=2.36 ;;
    *) echo "unknown arch $arch" >&2; exit 1 ;;
  esac
  echo "==> building $arch (glibc $glibc)"
  command -v rustup >/dev/null && rustup target add "$target" >/dev/null
  (cd "$ROOT" && cargo zigbuild --locked --release -p xrmc-desktop -p xrmc-cli --target "$target.$glibc")
  bin="$CARGO_TARGET_DIR/$target/release"
  stage="$OUT/xr-multicast-$VERSION-linux-$arch"
  rm -rf "$stage"
  mkdir -p "$stage"
  cp "$bin/xr-multicast" "$bin/xrmc" "$stage/"
  cp "$JAR" "$stage/scrcpy-server"
  cp "$ROOT/build/icon.png" "$ROOT/build/icon.svg" "$stage/"
  cp "$ROOT/packaging/linux/70-xr-multicast-adb.rules" "$stage/"
  cp "$ROOT/packaging/native/install.sh" "$stage/"
  cp "${docs[@]}" "$stage/"
  tar -C "$OUT" -czf "$stage.tar.gz" "$(basename "$stage")"
  echo "    $stage.tar.gz ($(du -h "$stage.tar.gz" | cut -f1))"
  if [[ $DEB == 1 ]]; then
    make_deb "$arch" "$stage" "$glibc"
  fi
done
