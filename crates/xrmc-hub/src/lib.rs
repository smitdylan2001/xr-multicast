//! Wires the platform-independent core ([`xrmc_core`]) to the real ADB client
//! ([`xrmc_adb`]) and scrcpy sessions ([`xrmc_scrcpy`]).
//!
//! Every app (desktop, Android, headless hub, CLI) builds a [`Hub`]: device
//! discovery/verification and stream management backed by real devices.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc};
use xrmc_adb::{AdbDevice, AdbHost, AdbKey, AdbRegistry, AnyDevice, RegistryOptions};
use xrmc_core::codec_policy::EncoderInfo as CoreEncoder;
use xrmc_core::config::Codec;
use xrmc_core::device_info::{self, TransportEntry as CoreTransport};
use xrmc_core::device_manager::{AdbBackend, DeviceManagerConfig, LanNetwork};
use xrmc_core::events::{self, CoreEvent, VideoPacket};
use xrmc_core::stream_manager::{
    SessionConfig as CoreSessionConfig, SessionControl, SessionEvent as CoreEvent2, SessionStart,
    StreamBackend, StreamManagerConfig,
};
use xrmc_core::view_size::FrameSize;
use xrmc_core::{CoreError, DeviceManager, SharedStore, StreamManager};
use xrmc_scrcpy::{
    Crop, ServerOptions, Session, SessionEvent as ScrcpyEvent, SessionHandle, VideoCodec,
};

// ------------------------------------------------------------------ ADB backend

/// [`AdbBackend`] over [`AdbRegistry`]: adb-server devices plus our own direct
/// USB/TCP connections.
#[derive(Clone)]
pub struct RegistryBackend {
    registry: AdbRegistry,
}

impl RegistryBackend {
    pub fn new(registry: AdbRegistry) -> Self {
        RegistryBackend { registry }
    }

    pub fn registry(&self) -> &AdbRegistry {
        &self.registry
    }
}

impl AdbBackend for RegistryBackend {
    type Device = AnyDevice;

    async fn transports(&self) -> xrmc_adb::Result<Vec<CoreTransport>> {
        let list = self.registry.transports().await?;
        Ok(list
            .into_iter()
            .map(|t| CoreTransport {
                serial: t.serial,
                state: t.state,
                network: t.network,
                props: t.props,
            })
            .collect())
    }

    async fn device(&self, adb_serial: &str) -> xrmc_adb::Result<AnyDevice> {
        self.registry.device(adb_serial).await
    }

    async fn connect(&self, address: &str, timeout: Duration) -> xrmc_adb::Result<()> {
        self.registry.connect(address, timeout).await
    }

    async fn disconnect(&self, address: &str) -> xrmc_adb::Result<()> {
        self.registry.disconnect(address).await
    }
}

// --------------------------------------------------------------- stream backend

/// [`StreamBackend`] that runs scrcpy-server sessions on devices from the registry.
#[derive(Clone)]
pub struct ScrcpyBackend {
    registry: AdbRegistry,
    jar: Option<Arc<Vec<u8>>>,
}

impl ScrcpyBackend {
    /// `jar` is the scrcpy-server binary (bundled with the app); `None` when missing.
    pub fn new(registry: AdbRegistry, jar: Option<Arc<Vec<u8>>>) -> Self {
        ScrcpyBackend { registry, jar }
    }

    fn jar(&self) -> Result<Arc<Vec<u8>>, CoreError> {
        self.jar.clone().ok_or(CoreError::ServerMissing)
    }
}

/// Stops a running scrcpy session.
pub struct ScrcpyControl {
    handle: SessionHandle,
}

impl SessionControl for ScrcpyControl {
    fn stop(&self) {
        self.handle.request_stop();
    }
}

fn scrcpy_codec(codec: Codec) -> VideoCodec {
    match codec {
        Codec::H264 => VideoCodec::H264,
        Codec::H265 => VideoCodec::H265,
        Codec::Av1 => VideoCodec::Av1,
    }
}

/// `"w:h:x:y"` (the format the core uses for eye crops).
fn parse_crop(s: &str) -> Option<Crop> {
    let v: Vec<u32> = s
        .split(':')
        .map(|p| p.trim().parse().ok())
        .collect::<Option<_>>()?;
    match v[..] {
        [width, height, x, y] => Some(Crop {
            width,
            height,
            x,
            y,
        }),
        _ => None,
    }
}

/// scrcpy-server options for a core session config.
pub fn server_options(config: &CoreSessionConfig) -> ServerOptions {
    let mut o = ServerOptions::new();
    o.codec = scrcpy_codec(config.codec);
    o.max_size = config.max_size;
    o.video_bit_rate = config.bit_rate;
    o.max_fps = config.max_fps;
    o.video_encoder = config.encoder.clone();
    o.crop = config.crop.as_deref().and_then(parse_crop);
    o.codec_options = config.codec_options.clone();
    o
}

fn adb_err(e: xrmc_adb::AdbError) -> CoreError {
    CoreError::Adb(e)
}

impl StreamBackend for ScrcpyBackend {
    type Session = ScrcpyControl;

    fn server_available(&self) -> bool {
        self.jar.is_some()
    }

    async fn list_encoders(&self, adb_serial: &str) -> Result<Vec<CoreEncoder>, CoreError> {
        let jar = self.jar()?;
        let device = self.registry.device(adb_serial).await.map_err(adb_err)?;
        let list = xrmc_scrcpy::list_encoders(&device, &jar)
            .await
            .map_err(|e| CoreError::Other(e.to_string()))?;
        Ok(list
            .into_iter()
            .map(|e| CoreEncoder {
                codec: e.codec,
                name: e.name,
                hw: e.hw,
                alias: e.alias,
            })
            .collect())
    }

    async fn panel_size(&self, adb_serial: &str) -> Result<Option<FrameSize>, CoreError> {
        let device = self.registry.device(adb_serial).await.map_err(adb_err)?;
        let out = device
            .shell(device_info::WM_SIZE_COMMAND)
            .await
            .map_err(adb_err)?;
        Ok(device_info::parse_wm_size(&String::from_utf8_lossy(&out)))
    }

    async fn start_session(
        &self,
        adb_serial: &str,
        config: CoreSessionConfig,
    ) -> Result<SessionStart<ScrcpyControl>, CoreError> {
        let jar = self.jar()?;
        let device = self.registry.device(adb_serial).await.map_err(adb_err)?;
        let (handle, mut rx) = Session::start(&device, &jar, server_options(&config))
            .await
            .map_err(|e| CoreError::Other(e.to_string()))?;
        // Translate scrcpy session events into the core's session events.
        let (tx, events) = mpsc::channel(256);
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                let mapped = match ev {
                    ScrcpyEvent::DeviceName(name) => Some(CoreEvent2::Device(name)),
                    ScrcpyEvent::VideoSize { width, height, .. } => {
                        Some(CoreEvent2::Session { width, height })
                    }
                    ScrcpyEvent::Packet(p) => Some(CoreEvent2::Packet(VideoPacket {
                        config: p.config,
                        key: p.key,
                        pts: p.pts,
                        data: p.data,
                    })),
                    ScrcpyEvent::Log(line) => Some(CoreEvent2::Log(line)),
                    ScrcpyEvent::Codec(_) => None,
                    ScrcpyEvent::Ended(err) => {
                        if let Some(e) = err {
                            let _ = tx.send(CoreEvent2::Error(e.to_string())).await;
                        }
                        let _ = tx.send(CoreEvent2::End).await;
                        break;
                    }
                };
                if let Some(m) = mapped
                    && tx.send(m).await.is_err()
                {
                    break;
                }
            }
        });
        // Keep the device connection alive as long as the session runs.
        drop(device);
        Ok(SessionStart {
            session: ScrcpyControl { handle },
            events,
        })
    }
}

// -------------------------------------------------------------------------- hub

/// Where the hub keeps its files.
#[derive(Debug, Clone)]
pub struct HubPaths {
    /// RSA key used to authenticate with headsets (created on first run).
    pub adb_key: PathBuf,
    /// Settings and verified headsets (same format as the Electron app's `xr-multicast.json`).
    pub store: PathBuf,
    /// The scrcpy-server binary (version [`xrmc_scrcpy::SCRCPY_VERSION`]).
    pub scrcpy_server: Option<PathBuf>,
}

impl HubPaths {
    /// Default locations in the per-user config directory (`$XDG_CONFIG_HOME`,
    /// `~/Library/Application Support`, `%APPDATA%`), next to the Electron app's data.
    pub fn default_for_user() -> Option<HubPaths> {
        let dir = config_dir()?.join("XR MultiCast");
        Some(HubPaths {
            adb_key: dir.join("adbkey"),
            store: dir.join("xr-multicast.json"),
            scrcpy_server: None,
        })
    }
}

fn config_dir() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(p));
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA").map(PathBuf::from)
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"))
    }
}

/// Options for [`Hub::open`].
#[derive(Debug, Clone)]
pub struct HubOptions {
    pub paths: HubPaths,
    pub registry: RegistryOptions,
    pub devices: DeviceManagerConfig,
    pub streams: StreamManagerConfig,
}

impl HubOptions {
    pub fn new(paths: HubPaths) -> Self {
        HubOptions {
            paths,
            registry: RegistryOptions::default(),
            devices: DeviceManagerConfig::default(),
            streams: StreamManagerConfig::default(),
        }
    }
}

pub type Devices = DeviceManager<RegistryBackend, LanNetwork>;
pub type Streams = StreamManager<ScrcpyBackend, Devices>;

/// A running XR MultiCast hub: devices + streams + one event channel.
pub struct Hub {
    pub registry: AdbRegistry,
    pub devices: Devices,
    pub streams: Streams,
    pub store: SharedStore,
    events: events::EventSender,
}

impl Hub {
    /// Load (or create) the ADB key and store, and set up the managers.
    /// Call [`Hub::start`] to begin polling.
    pub fn open(options: HubOptions) -> std::io::Result<Hub> {
        let key = AdbKey::load_or_generate(&options.paths.adb_key)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let registry = AdbRegistry::new(AdbHost::new(key), options.registry);
        let jar = match &options.paths.scrcpy_server {
            Some(p) => Some(Arc::new(read_file(p)?)),
            None => None,
        };
        let store = SharedStore::new(xrmc_core::Store::open(&options.paths.store));
        let (events, _) = events::channel(1024);
        let devices = DeviceManager::new(
            RegistryBackend::new(registry.clone()),
            LanNetwork::default(),
            store.clone(),
            events.clone(),
            options.devices,
        );
        let streams = StreamManager::new(
            ScrcpyBackend::new(registry.clone(), jar),
            devices.clone(),
            store.clone(),
            events.clone(),
            options.streams,
        );
        Ok(Hub {
            registry,
            devices,
            streams,
            store,
            events,
        })
    }

    /// Start background polling, info refresh and Wi-Fi auto-reconnect.
    pub fn start(&self) -> xrmc_core::device_manager::BackgroundTasks {
        self.devices.start()
    }

    /// Subscribe to device, stream, packet and notice events.
    pub fn subscribe(&self) -> broadcast::Receiver<CoreEvent> {
        self.events.subscribe()
    }
}

fn read_file(p: &Path) -> std::io::Result<Vec<u8>> {
    std::fs::read(p).map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", p.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop_strings_parse() {
        assert_eq!(
            parse_crop("1832:1920:0:0"),
            Some(Crop {
                width: 1832,
                height: 1920,
                x: 0,
                y: 0
            })
        );
        assert_eq!(parse_crop("1:2:3"), None);
        assert_eq!(parse_crop("a:b:c:d"), None);
    }

    #[test]
    fn session_config_maps_to_server_options() {
        let cfg = CoreSessionConfig {
            codec: Codec::H265,
            encoder: Some("c2.qti.hevc.encoder".into()),
            max_size: 1280,
            bit_rate: 3_000_000,
            max_fps: 60,
            crop: Some("2064:2208:0:0".into()),
            codec_options: vec!["profile=2".into()],
        };
        let o = server_options(&cfg);
        assert_eq!(o.codec, VideoCodec::H265);
        assert_eq!(o.max_size, 1280);
        assert_eq!(o.video_bit_rate, 3_000_000);
        assert_eq!(o.video_encoder.as_deref(), Some("c2.qti.hevc.encoder"));
        assert!(o.crop.is_some());
        let args = o.to_args().join(" ");
        assert!(args.contains("video_codec=h265"), "{args}");
        assert!(args.contains("profile=2"), "{args}");
    }
}
