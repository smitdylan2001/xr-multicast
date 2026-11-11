#!/usr/bin/env bash
# Install XR MultiCast for the current user (no root needed):
#   ~/.local/opt/xr-multicast/                        the app (from dist/linux-unpacked)
#   ~/.local/opt/xr-multicast/xr-multicast-launcher        start script used by the menu entry
#   ~/.local/bin/xr-multicast                         command-line launcher
#   ~/.local/share/applications/xr-multicast.desktop  app-grid / dock entry
#   ~/.local/share/icons/hicolor/...             icons
#
# Usage:
#   bash packaging/linux/install-user.sh             install / update
#   bash packaging/linux/install-user.sh --system    one-time system setup (sudo): AppArmor profile
#                                                    for the Chromium sandbox + USB rule for all headsets
#   bash packaging/linux/install-user.sh --uninstall
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SRC="$ROOT/dist/linux-unpacked"
DEST="$HOME/.local/opt/xr-multicast"
BIN="$HOME/.local/bin"
APPS="$HOME/.local/share/applications"
ICONS="$HOME/.local/share/icons/hicolor"
RULE_SRC="$ROOT/packaging/linux/70-xr-multicast-adb.rules"
RULE_DEST="/etc/udev/rules.d/70-xr-multicast-adb.rules"
# One profile per user, so several accounts on one machine don't clash.
PROFILE_NAME="xr-multicast-user-$(id -un)"
PROFILE_DEST="/etc/apparmor.d/$PROFILE_NAME"

refresh_caches() {
  update-desktop-database "$APPS" >/dev/null 2>&1 || true
  gtk-update-icon-cache -f -t "$ICONS" >/dev/null 2>&1 || true
}

install_udev() {
  echo "• USB access rule for all ADB headsets → $RULE_DEST"
  sudo install -m 0644 "$RULE_SRC" "$RULE_DEST"
  sudo udevadm control --reload-rules
  sudo udevadm trigger --subsystem-match=usb --action=change
}

install_apparmor() {
  # Ubuntu 24.04+ only lets programs create user namespaces (which Chromium's
  # sandbox needs) if an AppArmor profile allows it. Same profile the .deb ships.
  if [[ ! -d /etc/apparmor.d ]]; then
    echo "• AppArmor not present — no profile needed"
    return
  fi
  echo "• AppArmor profile for the Chromium sandbox → $PROFILE_DEST"
  local tmp
  tmp="$(mktemp)"
  cat > "$tmp" <<EOF
abi <abi/4.0>,
include <tunables/global>

profile $PROFILE_NAME "$DEST/xr-multicast" flags=(unconfined) {
  userns,

  include if exists <local/$PROFILE_NAME>
}
EOF
  sudo install -m 0644 "$tmp" "$PROFILE_DEST"
  rm -f "$tmp"
  sudo apparmor_parser --replace --write-cache --skip-read-cache "$PROFILE_DEST"
}

if [[ "${1:-}" == "--uninstall" ]]; then
  rm -rf "$DEST"
  rm -f "$BIN/xr-multicast" "$APPS/xr-multicast.desktop"
  rm -f "$ICONS/512x512/apps/xr-multicast.png" "$ICONS/scalable/apps/xr-multicast.svg"
  refresh_caches
  echo "XR MultiCast removed. (Saved headsets remain in ~/.config/XR MultiCast/ — delete that folder to reset.)"
  if [[ -f "$RULE_DEST" || -f "$PROFILE_DEST" ]]; then
    echo "System files from --system are still installed. Remove them with:"
    [[ -f "$PROFILE_DEST" ]] && echo "  sudo apparmor_parser --remove $PROFILE_DEST; sudo rm $PROFILE_DEST"
    [[ -f "$RULE_DEST" ]] && echo "  sudo rm $RULE_DEST && sudo udevadm control --reload-rules"
  fi
  exit 0
fi

if [[ "${1:-}" == "--system" || "${1:-}" == "--udev" ]]; then
  echo "One-time system setup for XR MultiCast (asks for your password):"
  install_apparmor
  install_udev
  echo "Done. Re-plug any connected headset and restart XR MultiCast."
  exit 0
fi

if [[ ! -x "$SRC/xr-multicast" ]]; then
  echo "Build not found at $SRC. Run: npm run install:linux" >&2
  exit 1
fi

echo "Installing XR MultiCast to $DEST"
rm -rf "$DEST.new"
mkdir -p "$(dirname "$DEST")"
cp -a "$SRC" "$DEST.new"
rm -rf "$DEST"
mv "$DEST.new" "$DEST"

# Launcher: Chromium aborts at startup ("SUID sandbox helper ... not configured
# correctly") when Ubuntu restricts user namespaces and no AppArmor profile
# allows them. Until --system has installed the profile, start without the
# Chromium sandbox instead of failing silently from the app grid.
cat > "$DEST/xr-multicast-launcher" <<EOF
#!/usr/bin/env bash
restricted="\$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns 2>/dev/null || echo 0)"
if [[ "\$restricted" == "1" && ! -f "$PROFILE_DEST" ]]; then
  exec "$DEST/xr-multicast" --no-sandbox "\$@"
fi
exec "$DEST/xr-multicast" "\$@"
EOF
chmod 0755 "$DEST/xr-multicast-launcher"

mkdir -p "$BIN" "$APPS" "$ICONS/512x512/apps" "$ICONS/scalable/apps"
ln -sf "$DEST/xr-multicast-launcher" "$BIN/xr-multicast"
install -m 0644 "$ROOT/build/icon.png" "$ICONS/512x512/apps/xr-multicast.png"
install -m 0644 "$ROOT/build/icon.svg" "$ICONS/scalable/apps/xr-multicast.svg"

cat > "$APPS/xr-multicast.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=XR MultiCast
GenericName=VR Headset Casting
Comment=Cast and monitor VR headset screens over USB or Wi-Fi
Exec="$DEST/xr-multicast-launcher" %U
TryExec=$DEST/xr-multicast-launcher
Icon=xr-multicast
Terminal=false
Categories=Utility;
Keywords=VR;XR;Quest;Meta;Pico;Vive;headset;cast;mirror;scrcpy;
StartupNotify=true
StartupWMClass=xr-multicast
EOF
chmod 0644 "$APPS/xr-multicast.desktop"
refresh_caches

echo "Done. Launch \"XR MultiCast\" from the app grid, or run: xr-multicast"
if [[ ! -f "$PROFILE_DEST" || ! -f "$RULE_DEST" ]]; then
  echo
  echo "Recommended one-time system setup (full Chromium sandbox + USB access for every headset brand):"
  echo "  bash packaging/linux/install-user.sh --system"
fi
