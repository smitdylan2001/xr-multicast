#!/usr/bin/env bash
# Install the native XR MultiCast app for the current user (no root needed).
# Run from the unpacked release folder (or packaging/native with a built binary).
#
#   ./install.sh                 install / update (app menu entry + `xr-multicast` command)
#   ./install.sh --kiosk         also start it fullscreen at login (touchscreen hub / SBC)
#   ./install.sh --udev          also install the USB rule for all ADB headsets (sudo)
#   ./install.sh --uninstall
set -euo pipefail

SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEST="$HOME/.local/opt/xr-multicast-native"  # kept from the preview builds (updates in place)
BIN="$HOME/.local/bin"
APPS="$HOME/.local/share/applications"
ICONS="$HOME/.local/share/icons/hicolor"
AUTOSTART="$HOME/.config/autostart/xr-multicast.desktop"
RULE_DEST="/etc/udev/rules.d/70-xr-multicast-adb.rules"

kiosk=0
udev=0
for a in "$@"; do
  case "$a" in
    --kiosk) kiosk=1 ;;
    --udev) udev=1 ;;
    --uninstall)
      rm -rf "$DEST"
      rm -f "$BIN/xr-multicast" "$APPS/xr-multicast.desktop" "$AUTOSTART"
      rm -f "$BIN/xr-multicast-native" "$APPS/xr-multicast-native.desktop" "$HOME/.config/autostart/xr-multicast-native.desktop"
      update-desktop-database "$APPS" >/dev/null 2>&1 || true
      echo "Native XR MultiCast removed (settings in ~/.config/XR MultiCast/ are kept)."
      exit 0
      ;;
    *) echo "unknown option $a" >&2; exit 1 ;;
  esac
done

[[ -x "$SRC/xr-multicast" ]] || { echo "xr-multicast binary not found next to install.sh" >&2; exit 1; }

echo "Installing native XR MultiCast to $DEST"
mkdir -p "$DEST" "$BIN" "$APPS" "$ICONS/512x512/apps" "$ICONS/scalable/apps"
for f in xr-multicast xrmc scrcpy-server icon.png icon.svg 70-xr-multicast-adb.rules LICENSE THIRD_PARTY_NOTICES.md; do
  [[ -e "$SRC/$f" ]] && install -m "$([[ -x "$SRC/$f" ]] && echo 0755 || echo 0644)" "$SRC/$f" "$DEST/$f"
done
# Replaces the preview entry ("XR MultiCast (native)") and the legacy Electron app's
# launcher/menu entry; the Electron files in ~/.local/opt/xr-multicast are left alone.
rm -f "$BIN/xr-multicast-native" "$APPS/xr-multicast-native.desktop"
if [[ -f "$HOME/.config/autostart/xr-multicast-native.desktop" ]]; then
  rm -f "$HOME/.config/autostart/xr-multicast-native.desktop"
  kiosk=1
fi
ln -sfn "$DEST/xr-multicast" "$BIN/xr-multicast"
install -m 0644 "$DEST/icon.png" "$ICONS/512x512/apps/xr-multicast.png"
[[ -f "$DEST/icon.svg" ]] && install -m 0644 "$DEST/icon.svg" "$ICONS/scalable/apps/xr-multicast.svg"

entry() {
  cat <<EOF
[Desktop Entry]
Type=Application
Name=XR MultiCast
GenericName=VR Headset Casting
Comment=Cast and monitor VR headset screens over USB or Wi-Fi
Exec="$DEST/xr-multicast" $1
Icon=xr-multicast
Terminal=false
Categories=Utility;
Keywords=VR;XR;Quest;Meta;Pico;Vive;headset;cast;mirror;
StartupNotify=true
EOF
}
entry "" > "$APPS/xr-multicast.desktop"
update-desktop-database "$APPS" >/dev/null 2>&1 || true
gtk-update-icon-cache -f -t "$ICONS" >/dev/null 2>&1 || true

if [[ $kiosk == 1 ]]; then
  mkdir -p "$(dirname "$AUTOSTART")"
  entry "--kiosk" > "$AUTOSTART"
  echo "Kiosk mode: XR MultiCast starts fullscreen at login (remove $AUTOSTART to undo)."
fi

if [[ $udev == 1 ]]; then
  echo "Installing the USB rule for ADB headsets → $RULE_DEST (needs sudo)"
  sudo install -m 0644 "$DEST/70-xr-multicast-adb.rules" "$RULE_DEST"
  sudo udevadm control --reload-rules
  sudo udevadm trigger --subsystem-match=usb --action=change
fi

echo "Done. Start \"XR MultiCast\" from the app menu, or run: xr-multicast"
