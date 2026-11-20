# Security policy

## Reporting a vulnerability

Please **don't open a public issue** for security problems. Report them privately:

- GitHub: **[Report a vulnerability](https://github.com/smitdylan2001/xr-multicast/security/advisories/new)**
  (Security tab → Report a vulnerability), or
- email **[info@dunktechnologies.com](mailto:info@dunktechnologies.com)** with "XR MultiCast security" in the subject.

Include the version, platform, steps to reproduce and the impact you see. We aim to reply
within a week, and will credit you in the release notes unless you'd rather not be named.

Fixes go into the latest release; older versions aren't patched.

## What XR MultiCast does with your data

- Everything stays on your local network. There is no cloud service, account or telemetry.
- Settings and the list of verified headsets are stored in `xr-multicast.json` in the user's
  config folder (on Android: the app's private storage).
- Video goes directly from each headset to the hub and is never recorded, except for snapshots
  you save yourself.

## ADB keys and headset access

XR MultiCast controls headsets through ADB (Android Debug Bridge), the same developer
interface that tools like SideQuest and Android Studio use. Keep in mind:

- **The hub's ADB key is a credential.** XR MultiCast creates an RSA key pair, `adbkey`, next
  to its settings file. Every headset on which you tick *Always allow from this computer*
  trusts that key: whoever has the file can run shell commands on those headsets and see their
  screens. Protect it like a password, don't share it between untrusted machines, and don't
  commit it anywhere. On desktop, XR MultiCast also offers Google adb's own keys
  (`~/.android/adbkey`, `ADB_VENDOR_KEYS`) when they exist, so already-trusted headsets don't
  prompt again; it only reads them.
- **Revoke access** on a headset with Developer options → *Revoke USB debugging
  authorisations*, then re-verify it with the hubs you trust.
- **Network debugging** (Wi-Fi casting) opens the headset's ADB port (usually 5555) on the
  local network. Connections still need an authorised key, but use it only on networks you
  trust, not on public or guest Wi-Fi. It switches off when the headset reboots.
- **USB-first verification** (on by default) means only headsets you plugged in and verified
  can cast. Devices that merely answer on the network are listed but can't cast.
- **scrcpy-server** is pushed to `/data/local/tmp` on each headset while casting and runs with
  shell permissions. XR MultiCast uses the unmodified upstream v5.0.1 release.
- **Linux USB rule:** `70-xr-multicast-adb.rules` gives the logged-in user (and the `plugdev`
  group) access to the ADB interface of USB devices. On shared machines, review who is in
  `plugdev`.

## Legacy Electron app

The 1.x Electron app in `src/` is no longer released. Its remote dashboard serves plain
`http://` on the LAN with PIN pairing; only enable it on trusted networks.
