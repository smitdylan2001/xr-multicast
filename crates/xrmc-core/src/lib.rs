//! Platform-independent core of XR MultiCast, shared by the desktop apps
//! (Windows, macOS, Linux), Android and headless hubs.
//!
//! It is a port of the Electron app's main-process logic (the reference
//! implementation) and keeps its behaviour, messages and file formats:
//!
//! * pure policy: [`brands`] (headset detection and lens presets), [`layout`]
//!   (eye-view layout detection), [`view_size`] (stream resolution matching),
//!   [`codec_policy`] (codec / 10-bit / quality choice);
//! * data: [`settings`] and [`store`] (compatible with the Electron
//!   `xr-multicast.json`), [`device_info`] (adb shell commands and parsers);
//! * I/O: [`discovery`] (LAN scan, mDNS);
//! * orchestration: [`device_manager::DeviceManager`] and
//!   [`stream_manager::StreamManager`], reporting through [`events::CoreEvent`].
//!
//! ADB access goes through the [`xrmc_adb::AdbDevice`] trait (plus
//! [`device_manager::AdbBackend`] for host-level verbs) and scrcpy sessions through
//! [`stream_manager::StreamBackend`], so everything here can be tested with fakes.

pub mod brands;
pub mod codec_policy;
pub mod config;
pub mod device_info;
pub mod device_manager;
pub mod discovery;
pub mod events;
mod js;
pub mod layout;
pub mod settings;
pub mod store;
pub mod stream_manager;
pub mod view_size;

pub use config::{
    Codec, CodecPreference, QualityKey, QualityPreset, TransportKind, TransportPreference,
};
pub use device_manager::{DeviceManager, DeviceSnapshot, HeadsetSnapshot};
pub use events::{CoreEvent, Notice, NoticeLevel, VideoPacket};
pub use settings::Settings;
pub use store::{HeadsetRecord, SharedStore, Store};
pub use stream_manager::{StreamManager, StreamState, StreamStatus};

/// Errors reported to the user. Messages match the Electron app.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("Headset is not connected.")]
    NotConnected,
    #[error("Verify \"{name}\" over USB before casting (Settings → require USB verification).")]
    NotVerified { name: String },
    #[error("scrcpy-server is missing. Reinstall the app.")]
    ServerMissing,
    #[error("Verification requires a USB connection. Plug the headset in with a USB data cable.")]
    UsbRequired,
    #[error("The headset did not respond over USB.")]
    NoResponse,
    #[error("Only verified headsets can be edited.")]
    NotTrusted,
    #[error("Invalid address \"{0}\". Use e.g. 192.168.1.50 or 192.168.1.50:5555")]
    InvalidAddress(String),
    #[error(transparent)]
    Adb(#[from] xrmc_adb::AdbError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}
