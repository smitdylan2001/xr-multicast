//! In-process transport registry: the equivalent of `adb devices -l`,
//! `adb connect` and `adb disconnect`, without an adb binary.
//!
//! [`AdbRegistry`] merges three sources:
//! * devices listed by a running adb server (desktop coexistence; those stay
//!   with the server because it owns their USB interfaces),
//! * USB devices it enumerates and connects to directly (when no server holds them),
//! * direct TCP connections made with [`AdbRegistry::connect`].
//!
//! Direct connections are set up in background tasks, so a headset waiting
//! for "Allow USB debugging" shows up as `unauthorized` (like with adb) and
//! becomes `device` once the user accepts.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tokio::task::AbortHandle;

use crate::connection::{AdbConnection, ConnectOptions, HandshakeStage};
use crate::device::{AnyDevice, DirectDevice, TransportKind};
use crate::host::{AdbHost, DEFAULT_TCP_PORT, normalize_address};
use crate::{AdbError, Result};

/// What the registry does.
#[derive(Debug, Clone)]
pub struct RegistryOptions {
    /// Include devices listed by a running adb server.
    pub use_server: bool,
    /// Enumerate USB devices and connect to them directly (devices the adb
    /// server lists are left to the server). Ignored on Android, where USB
    /// devices are added with [`AdbRegistry::add_usb_fd`].
    pub direct_usb: bool,
    /// [`AdbRegistry::connect`] opens direct TCP connections; otherwise it asks
    /// the adb server (`host:connect`).
    pub direct_tcp: bool,
    /// How long a direct connection waits for the user to accept the prompt
    /// before giving up (it is retried on a later [`AdbRegistry::transports`] call).
    pub auth_timeout: Duration,
    /// Delay before retrying a failed direct USB connection.
    pub retry_delay: Duration,
}

impl Default for RegistryOptions {
    fn default() -> Self {
        RegistryOptions {
            use_server: true,
            direct_usb: true,
            direct_tcp: true,
            auth_timeout: Duration::from_secs(300),
            retry_delay: Duration::from_secs(5),
        }
    }
}

/// Where a transport comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportSource {
    /// Listed by the adb server.
    Server,
    /// Our own connection (USB or TCP).
    Direct,
}

/// One transport, like a line of `adb devices -l`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportEntry {
    /// adb serial: USB serial, `ip:port`, or (server) an mDNS name.
    pub serial: String,
    /// `device`, `connecting`, `authorizing`, `unauthorized`, `offline`,
    /// `no permissions`, `busy`, ... (adb server states are passed through).
    pub state: String,
    /// Network (TCP / mDNS) transport rather than USB.
    pub network: bool,
    /// `product`, `model`, `device` (from the device banner), `usb`, `transport_id`, ...
    pub props: BTreeMap<String, String>,
    /// Server or direct.
    pub source: TransportSource,
    /// Last error for failed direct transports.
    pub error: Option<String>,
}

impl TransportEntry {
    /// Online and authorised.
    pub fn is_ready(&self) -> bool {
        self.state == "device"
    }
}

/// Tracks devices from the adb server and direct USB/TCP connections.
#[derive(Clone)]
pub struct AdbRegistry {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for AdbRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdbRegistry")
            .field("options", &self.inner.options)
            .field(
                "direct",
                &lock(&self.inner.slots).keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

struct Inner {
    host: AdbHost,
    options: RegistryOptions,
    slots: Mutex<HashMap<String, Slot>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        let slots = self
            .slots
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for slot in slots.values_mut() {
            if let Some(task) = slot.task.take() {
                task.abort();
            }
        }
    }
}

struct Slot {
    kind: TransportKind,
    state: SlotState,
    task: Option<AbortHandle>,
    /// Bumped for every connection attempt, so stale tasks don't overwrite newer state.
    attempt: u64,
    props: BTreeMap<String, String>,
}

enum SlotState {
    Connecting,
    Authorizing,
    Unauthorized,
    Ready(DirectDevice),
    Failed {
        state: &'static str,
        error: String,
        /// When it failed (USB retries are spaced by `retry_delay`; no enumeration on Android).
        #[cfg_attr(target_os = "android", allow(dead_code))]
        at: Instant,
    },
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `AdbError` is not `Clone` (it wraps `io::Error`); copy it preserving the variant and kind.
fn copy_error(e: &AdbError) -> AdbError {
    match e {
        AdbError::Io(io) => AdbError::Io(std::io::Error::new(io.kind(), io.to_string())),
        AdbError::Protocol(m) => AdbError::Protocol(m.clone()),
        AdbError::Unauthorized => AdbError::Unauthorized,
        AdbError::Refused(m) => AdbError::Refused(m.clone()),
        AdbError::Disconnected => AdbError::Disconnected,
        AdbError::Timeout => AdbError::Timeout,
        AdbError::Other(m) => AdbError::Other(m.clone()),
    }
}

fn failure_state(e: &AdbError) -> &'static str {
    match e {
        AdbError::Unauthorized => "unauthorized",
        AdbError::Io(io)
            if crate::usb::is_busy(e) && io.kind() != std::io::ErrorKind::PermissionDenied =>
        {
            "busy"
        }
        AdbError::Io(io) if io.kind() == std::io::ErrorKind::PermissionDenied => "no permissions",
        _ => "offline",
    }
}

impl AdbRegistry {
    /// A registry using `host`'s key, connection options and adb server.
    pub fn new(host: AdbHost, options: RegistryOptions) -> AdbRegistry {
        AdbRegistry {
            inner: Arc::new(Inner {
                host,
                options,
                slots: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// The underlying host (key, options, adb server).
    pub fn host(&self) -> &AdbHost {
        &self.inner.host
    }

    /// Current transports from all sources. Also starts direct connections to
    /// newly plugged-in USB devices and drops lost ones; call it periodically
    /// (the Electron app polled every 2 s).
    pub async fn transports(&self) -> Result<Vec<TransportEntry>> {
        let opts = &self.inner.options;
        let server_list = if opts.use_server {
            self.inner.host.server().devices().await.ok()
        } else {
            None
        };
        let server_serials: HashSet<String> = server_list
            .iter()
            .flatten()
            .map(|d| d.serial.clone())
            .collect();

        #[cfg(not(target_os = "android"))]
        if opts.direct_usb {
            match self.inner.host.list_usb().await {
                Ok(usb) => self.sync_usb(usb, &server_serials),
                Err(e) => tracing::debug!("USB enumeration failed: {e}"),
            }
        }

        let mut entries = Vec::new();
        let mut direct_serials = HashSet::new();
        {
            let mut slots = lock(&self.inner.slots);
            // Forget direct connections that died (unplugged, Wi-Fi lost, adbd restarted).
            slots.retain(|_, slot| !matches!(&slot.state, SlotState::Ready(d) if d.is_closed()));
            for (serial, slot) in slots.iter() {
                let (state, error) = match &slot.state {
                    SlotState::Connecting => ("connecting", None),
                    SlotState::Authorizing => ("authorizing", None),
                    SlotState::Unauthorized => ("unauthorized", None),
                    SlotState::Ready(_) => ("device", None),
                    SlotState::Failed { state, error, .. } => (*state, Some(error.clone())),
                };
                // A device we could not open because the server holds it is listed by the server.
                if state == "busy" && server_serials.contains(serial) {
                    continue;
                }
                direct_serials.insert(serial.clone());
                entries.push(TransportEntry {
                    serial: serial.clone(),
                    state: state.to_string(),
                    network: slot.kind == TransportKind::Tcp,
                    props: slot.props.clone(),
                    source: TransportSource::Direct,
                    error,
                });
            }
        }
        for d in server_list.into_iter().flatten() {
            if direct_serials.contains(&d.serial) {
                continue;
            }
            entries.push(TransportEntry {
                network: d.is_network(),
                serial: d.serial,
                state: d.state,
                props: d.props,
                source: TransportSource::Server,
                error: None,
            });
        }
        entries.sort_by(|a, b| a.serial.cmp(&b.serial));
        Ok(entries)
    }

    #[cfg(not(target_os = "android"))]
    fn sync_usb(&self, usb: Vec<crate::usb::UsbDeviceInfo>, server_serials: &HashSet<String>) {
        let present: HashSet<String> = usb.iter().map(|d| d.adb_serial()).collect();
        let mut to_start = Vec::new();
        {
            let mut slots = lock(&self.inner.slots);
            // Unplugged USB devices.
            slots.retain(|serial, slot| {
                let keep = slot.kind != TransportKind::Usb || present.contains(serial);
                if !keep && let Some(task) = slot.task.take() {
                    task.abort();
                }
                keep
            });
            for info in usb {
                let serial = info.adb_serial();
                let owned_by_server = server_serials.contains(&serial);
                let start = match slots.get(&serial) {
                    None => !owned_by_server,
                    Some(slot) => match &slot.state {
                        SlotState::Failed { at, .. } => {
                            !owned_by_server && at.elapsed() >= self.inner.options.retry_delay
                        }
                        _ => false,
                    },
                };
                if start {
                    let attempt = slots.get(&serial).map_or(0, |s| s.attempt) + 1;
                    let mut props = BTreeMap::new();
                    props.insert("usb".to_string(), info.location());
                    if let Some(p) = &info.product {
                        props.insert("model".to_string(), p.replace(' ', "_"));
                    }
                    slots.insert(
                        serial.clone(),
                        Slot {
                            kind: TransportKind::Usb,
                            state: SlotState::Connecting,
                            task: None,
                            attempt,
                            props,
                        },
                    );
                    to_start.push((serial, info, attempt));
                }
            }
        }
        for (serial, info, attempt) in to_start {
            let weak = Arc::downgrade(&self.inner);
            let options = self.connect_options(&serial, attempt);
            let key = self.inner.host.key().clone();
            let task_serial = serial.clone();
            let handle = tokio::spawn(async move {
                let result = async {
                    let transport = crate::usb::UsbTransport::open(&info).await?;
                    AdbConnection::connect_usb(transport, Some(&key), &options).await
                }
                .await;
                finish_attempt(&weak, &task_serial, attempt, TransportKind::Usb, result);
            });
            self.set_task(&serial, attempt, handle.abort_handle());
        }
    }

    /// Add a USB device opened by the platform (Android `UsbManager`; see
    /// [`crate::usb::UsbTransport::from_fd`]). Returns the serial it is listed under.
    #[cfg(any(target_os = "android", target_os = "linux"))]
    pub fn add_usb_fd(&self, fd: std::os::fd::OwnedFd, serial: Option<String>) -> String {
        let serial = serial.filter(|s| !s.is_empty()).unwrap_or_else(|| {
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
            format!(
                "usb-fd-{}",
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            )
        });
        let attempt = self.begin(&serial, TransportKind::Usb);
        let weak = Arc::downgrade(&self.inner);
        let options = self.connect_options(&serial, attempt);
        let key = self.inner.host.key().clone();
        let task_serial = serial.clone();
        let handle = tokio::spawn(async move {
            let result = async {
                let transport =
                    crate::usb::UsbTransport::from_fd(fd, Some(task_serial.clone())).await?;
                AdbConnection::connect_usb(transport, Some(&key), &options).await
            }
            .await;
            finish_attempt(&weak, &task_serial, attempt, TransportKind::Usb, result);
        });
        self.set_task(&serial, attempt, handle.abort_handle());
        serial
    }

    /// Forget a direct transport (e.g. Android reported the USB device detached).
    pub fn remove(&self, serial: &str) {
        let slot = lock(&self.inner.slots).remove(serial);
        if let Some(mut slot) = slot {
            if let Some(task) = slot.task.take() {
                task.abort();
            }
            if let SlotState::Ready(device) = slot.state {
                device.connection().close();
            }
        }
    }

    /// A handle for one transport: our direct connection if we have one,
    /// otherwise the adb server's.
    pub async fn device(&self, serial: &str) -> Result<AnyDevice> {
        {
            let slots = lock(&self.inner.slots);
            if let Some(slot) = slots.get(serial) {
                match &slot.state {
                    SlotState::Ready(d) if !d.is_closed() => {
                        return Ok(AnyDevice::Direct(d.clone()));
                    }
                    SlotState::Unauthorized | SlotState::Authorizing => {
                        return Err(AdbError::Unauthorized);
                    }
                    _ => {}
                }
            }
        }
        if self.inner.options.use_server {
            return Ok(self.inner.host.connect_server(serial).await?.into());
        }
        Err(AdbError::Other(format!("device {serial} is not connected")))
    }

    /// Connect to a device in `adb tcpip` mode (`host` or `host:port`, default
    /// port 5555), like `adb connect`: returns once the device is connected or
    /// is waiting for the user to accept the prompt (listed as `unauthorized`).
    pub async fn connect(&self, address: &str, timeout: Duration) -> Result<()> {
        let addr = normalize_address(address, DEFAULT_TCP_PORT)?;
        if !self.inner.options.direct_tcp {
            self.inner.host.server().connect_device(&addr).await?;
            return Ok(());
        }
        {
            let slots = lock(&self.inner.slots);
            if let Some(slot) = slots.get(&addr) {
                match &slot.state {
                    SlotState::Ready(d) if !d.is_closed() => return Ok(()),
                    SlotState::Connecting | SlotState::Authorizing | SlotState::Unauthorized => {
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }
        let attempt = self.begin(&addr, TransportKind::Tcp);
        let (stage_tx, mut stage_rx) = watch::channel(HandshakeStage::Connecting);
        let mut options = self.connect_options(&addr, attempt);
        let inner_stage = options.on_stage.take();
        options.on_stage = Some(Arc::new(move |stage| {
            if let Some(cb) = &inner_stage {
                cb(stage);
            }
            stage_tx.send_replace(stage);
        }));
        let weak = Arc::downgrade(&self.inner);
        let key = self.inner.host.key().clone();
        let task_addr = addr.clone();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            let result =
                AdbConnection::connect_tcp(&task_addr, Some(&key), &options, timeout).await;
            let outcome = result.as_ref().map(|_| ()).map_err(copy_error);
            finish_attempt(&weak, &task_addr, attempt, TransportKind::Tcp, result);
            let _ = done_tx.send(outcome);
        });
        self.set_task(&addr, attempt, handle.abort_handle());

        tokio::select! {
            outcome = done_rx => {
                let outcome = outcome.unwrap_or(Err(AdbError::Disconnected));
                if outcome.is_err() && !matches!(outcome, Err(AdbError::Unauthorized)) {
                    // Like adb: a failed connect leaves no transport behind.
                    let mut slots = lock(&self.inner.slots);
                    if slots.get(&addr).is_some_and(|s| s.attempt == attempt) {
                        slots.remove(&addr);
                    }
                }
                outcome
            }
            // (An `Err` means the task ended and dropped the sender: `done_rx` answers then.)
            Ok(_) = stage_rx.wait_for(|s| *s == HandshakeStage::AwaitingUserApproval) => Ok(()),
        }
    }

    /// Drop a TCP transport, like `adb disconnect`: closes our direct
    /// connection, or asks the adb server if it owns the transport.
    pub async fn disconnect(&self, address: &str) -> Result<()> {
        let addr =
            normalize_address(address, DEFAULT_TCP_PORT).unwrap_or_else(|_| address.to_string());
        let had_direct = lock(&self.inner.slots).contains_key(&addr);
        if had_direct {
            self.remove(&addr);
            return Ok(());
        }
        if self.inner.options.use_server {
            self.inner.host.server().disconnect_device(&addr).await?;
        }
        Ok(())
    }

    fn begin(&self, serial: &str, kind: TransportKind) -> u64 {
        let mut slots = lock(&self.inner.slots);
        let previous = slots.remove(serial);
        let attempt = previous.as_ref().map_or(0, |s| s.attempt) + 1;
        if let Some(task) = previous.and_then(|mut s| s.task.take()) {
            task.abort();
        }
        slots.insert(
            serial.to_string(),
            Slot {
                kind,
                state: SlotState::Connecting,
                task: None,
                attempt,
                props: BTreeMap::new(),
            },
        );
        attempt
    }

    fn set_task(&self, serial: &str, attempt: u64, task: AbortHandle) {
        let mut slots = lock(&self.inner.slots);
        match slots.get_mut(serial) {
            Some(slot) if slot.attempt == attempt => slot.task = Some(task),
            _ => {}
        }
    }

    /// Host options with the registry's auth timeout and a stage hook updating the slot.
    fn connect_options(&self, serial: &str, attempt: u64) -> ConnectOptions {
        let mut options = self.inner.host.options().clone();
        options.auth_timeout = self.inner.options.auth_timeout;
        let weak: Weak<Inner> = Arc::downgrade(&self.inner);
        let serial = serial.to_string();
        let user_cb = options.on_stage.take();
        options.on_stage = Some(Arc::new(move |stage| {
            if let Some(cb) = &user_cb {
                cb(stage);
            }
            let Some(inner) = weak.upgrade() else { return };
            let mut slots = lock(&inner.slots);
            if let Some(slot) = slots.get_mut(&serial).filter(|s| s.attempt == attempt) {
                match stage {
                    HandshakeStage::Authenticating => slot.state = SlotState::Authorizing,
                    HandshakeStage::AwaitingUserApproval => slot.state = SlotState::Unauthorized,
                    HandshakeStage::Connecting | HandshakeStage::Connected => {}
                }
            }
        }));
        options
    }
}

fn finish_attempt(
    weak: &Weak<Inner>,
    serial: &str,
    attempt: u64,
    kind: TransportKind,
    result: Result<AdbConnection>,
) {
    let Some(inner) = weak.upgrade() else { return };
    let mut slots = lock(&inner.slots);
    let Some(slot) = slots.get_mut(serial).filter(|s| s.attempt == attempt) else {
        return;
    };
    slot.task = None;
    match result {
        Ok(conn) => {
            let banner = conn.banner();
            for (key, prop) in [
                ("product", "ro.product.name"),
                ("model", "ro.product.model"),
                ("device", "ro.product.device"),
            ] {
                if let Some(v) = banner.props.get(prop) {
                    slot.props.insert(key.to_string(), v.replace(' ', "_"));
                }
            }
            slot.state = SlotState::Ready(DirectDevice::new(conn, kind));
        }
        Err(e) => {
            tracing::debug!("direct connection to {serial} failed: {e}");
            slot.state = SlotState::Failed {
                state: failure_state(&e),
                error: e.to_string(),
                at: Instant::now(),
            };
        }
    }
}
