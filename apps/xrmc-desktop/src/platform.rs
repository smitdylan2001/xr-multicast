//! What differs between the platforms that run this UI (desktop and the
//! Android app): where USB devices come from, network discovery helpers,
//! file locations, fullscreen, keeping the screen on and the soft keyboard.
//!
//! The desktop implementation ([`Desktop`]) is the default. The Android app
//! (`apps/xrmc-android`) implements [`Platform`] with JNI calls into its
//! Java shell and hands it to [`crate::ui::AppOptions::platform`].

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui;
use xrmc_core::NoticeLevel;

/// What a platform gets once the hub is running.
#[derive(Clone)]
pub struct HubHandle {
    /// Add platform-opened USB devices ([`xrmc_adb::AdbRegistry::add_usb_fd`] on
    /// Android) and remove detached ones ([`xrmc_adb::AdbRegistry::remove`]).
    pub registry: xrmc_adb::AdbRegistry,
    /// The hub's tokio runtime (`add_usb_fd` spawns its connection task there).
    pub runtime: tokio::runtime::Handle,
}

impl std::fmt::Debug for HubHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubHandle").finish_non_exhaustive()
    }
}

/// Kind of text field, so the soft keyboard can show a suitable layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKind {
    /// Names and labels.
    Text,
    /// IP address with optional port (digits, dots and a colon).
    Address,
}

/// Something the platform wants to tell the user.
#[derive(Debug, Clone, PartialEq)]
pub struct PlatformNotice {
    pub level: NoticeLevel,
    pub message: String,
}

/// Platform services used by the shared UI. All methods have desktop defaults.
pub trait Platform: Send + Sync + std::fmt::Debug + 'static {
    /// Short platform name for diagnostics, e.g. `Linux`, `Android`.
    fn name(&self) -> String {
        std::env::consts::OS.to_string()
    }

    /// What the UI calls the machine running it ("computer", "phone or tablet").
    fn device_noun(&self) -> &'static str {
        "computer"
    }

    /// Phones and tablets: no window to resize, no fullscreen toggle, no quit button.
    fn is_mobile(&self) -> bool {
        false
    }

    /// Called once when the UI starts (keep `ctx` to wake the UI from other threads).
    fn start(&self, _ctx: &egui::Context) {}

    /// Called on the hub's runtime once the hub is open.
    fn hub_started(&self, _hub: HubHandle) {}

    /// Keep the display on (while streams are shown).
    fn keep_screen_on(&self, _on: bool) {}

    /// A network scan starts (`true`) or ends (`false`); Android holds a Wi-Fi
    /// multicast lock meanwhile so mDNS answers arrive.
    fn network_scan(&self, _active: bool) {}

    /// Called after every text field is shown (soft-keyboard bridge on Android).
    fn text_input(
        &self,
        _ctx: &egui::Context,
        _resp: &egui::Response,
        _value: &mut String,
        _kind: TextKind,
    ) {
    }

    /// Height of the on-screen keyboard in points (0 when hidden or not applicable).
    fn keyboard_height(&self) -> f32 {
        0.0
    }

    /// The back button was pressed with nothing left to close (Android: go to
    /// the home screen; streams keep running).
    fn back_at_root(&self) {}

    /// Messages for the user since the last call (USB permission denied, ...).
    fn poll_notices(&self) -> Vec<PlatformNotice> {
        Vec::new()
    }

    /// Where snapshots (PNG) are saved.
    fn snapshot_dir(&self) -> PathBuf {
        default_snapshot_dir()
    }

    /// Troubleshooting hint about USB for this platform (connection guide).
    fn usb_hint(&self) -> &'static str {
        default_usb_hint()
    }

    /// One line about the platform's USB/network support for Settings → About.
    fn about(&self) -> String {
        String::new()
    }
}

/// Windows, macOS and Linux.
#[derive(Debug, Default, Clone, Copy)]
pub struct Desktop;

impl Platform for Desktop {}

/// The default platform for this build.
pub fn default_platform() -> Arc<dyn Platform> {
    Arc::new(Desktop)
}

/// `~/Pictures/XR MultiCast`, else `~/XR MultiCast snapshots`.
pub fn default_snapshot_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let pictures = home.join("Pictures");
    if pictures.is_dir() {
        pictures.join("XR MultiCast")
    } else {
        home.join("XR MultiCast snapshots")
    }
}

fn default_usb_hint() -> &'static str {
    if cfg!(target_os = "linux") {
        "Linux: if a USB headset doesn't appear, install the USB rule (packaging/native/install.sh --udev, or the XR MultiCast .deb) and re-plug it. On a Raspberry Pi hub, use a powered USB hub for several headsets."
    } else if cfg!(target_os = "windows") {
        "Windows: direct USB needs a WinUSB-compatible ADB driver (Meta: Oculus ADB Drivers; others: Google USB Driver or the vendor's driver). Re-plug after installing."
    } else {
        "macOS: no driver needed. Use a USB-C data cable (some charge-only cables won't work)."
    }
}
