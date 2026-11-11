//! Pure-Rust ADB client for XR MultiCast.
//!
//! Talks the ADB wire protocol directly over USB and TCP (no Google `adb`
//! binary needed), and can also go through a running adb server
//! (`localhost:5037`) so it coexists with other tools.
//!
//! Other crates program against the [`AdbDevice`] trait so they work the same
//! over any transport (and with test fakes).
//!
//! # Overview
//!
//! * [`AdbHost`]: holds the host key ([`AdbKey`]) and [`ConnectOptions`]; lists
//!   USB devices and connects over USB, TCP or the adb server.
//! * [`DirectDevice`]: our own [`AdbConnection`] (USB or TCP) — handshake,
//!   RSA auth and stream multiplexing happen in this crate.
//! * [`ServerDevice`]: a device behind a running adb server.
//! * [`AnyDevice`]: either of the above; implements [`AdbDevice`] with
//!   [`AnyStream`] streams.
//! * [`AdbRegistry`]: `adb devices` / `adb connect` / `adb disconnect` in
//!   process — merges adb-server devices with direct USB and TCP connections.
//!
//! # Limitations
//!
//! * No TLS: Android 11+ "Wireless debugging" (pairing code, `_adb-tls-connect`)
//!   needs the `STLS` handshake, which is detected and reported as an error.
//!   Use USB or `adb tcpip` mode instead.
//! * Classic flow control only (one `OKAY` per `WRTE`); `delayed_ack` is not
//!   announced. Sync uses v1 `SEND` (no compression).
//! * Device-initiated streams (`adb reverse`) are refused.
//! * Windows: direct USB needs the ADB interface bound to WinUSB (Google USB
//!   driver or the vendor's ADB driver). On every desktop OS an adb server
//!   that is running owns the USB interfaces; use the server transport then.
//!
//! ```no_run
//! # async fn demo() -> xrmc_adb::Result<()> {
//! use xrmc_adb::{AdbDevice, AdbHost};
//! use std::time::Duration;
//!
//! let host = AdbHost::load_or_generate("/path/to/app-data/adbkey")?;
//! let device = host.connect_tcp("192.168.1.50:5555", Duration::from_secs(5)).await?;
//! let model = device.getprop("ro.product.model").await?;
//! device.push(b"hello", "/data/local/tmp/hello.txt", 0o644).await?;
//! let out = device.shell("cat /data/local/tmp/hello.txt").await?;
//! # let _ = (model, out);
//! # Ok(()) }
//! ```

use std::future::Future;

use tokio::io::{AsyncRead, AsyncWrite};

pub mod auth;
pub mod connection;
pub mod device;
pub mod host;
pub mod protocol;
pub mod registry;
pub mod server;
pub mod shell;
pub mod sync;
pub mod transport;
pub mod usb;

pub use auth::AdbKey;
pub use connection::{AdbConnection, AdbSocket, ConnectOptions, ConnectionInfo, HandshakeStage};
pub use device::{AnyDevice, AnyStream, DirectDevice, ServerDevice, TransportKind};
pub use host::{AdbHost, DEFAULT_TCP_PORT};
pub use protocol::DeviceBanner;
pub use registry::{AdbRegistry, RegistryOptions, TransportEntry, TransportSource};
pub use server::{AdbServer, ServerDeviceEntry};
pub use shell::ShellOutput;
pub use sync::FileStat;
pub use usb::{UsbDeviceInfo, UsbTransport};

/// Errors from ADB operations.
#[derive(Debug, thiserror::Error)]
pub enum AdbError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The device has not (yet) authorised this host's key: the user must accept
    /// "Allow USB debugging" on the headset.
    #[error("device not authorised: accept \"Allow USB debugging\" on the headset")]
    Unauthorized,
    /// The device refused to open a service (e.g. unknown service, socket not listening yet).
    #[error("service refused: {0}")]
    Refused(String),
    #[error("device disconnected")]
    Disconnected,
    #[error("timed out")]
    Timeout,
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, AdbError>;

/// A bidirectional byte stream to a service on the device (a shell, a local socket, ...).
/// Dropping or shutting it down closes the stream on the device.
pub trait AdbStream: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> AdbStream for T {}

/// One connected device (over USB, TCP, or through an adb server).
pub trait AdbDevice: Send + Sync {
    type Stream: AdbStream;

    /// Transport-level identifier: USB serial, `ip:port`, or adb-server serial.
    fn serial(&self) -> &str;

    /// Run a shell command to completion and return its output (stdout; stderr may be merged).
    fn shell(&self, command: &str) -> impl Future<Output = Result<Vec<u8>>> + Send;

    /// Start a long-running shell command and return its output stream.
    /// Closing the stream ends the command on the device.
    fn shell_stream(&self, command: &str) -> impl Future<Output = Result<Self::Stream>> + Send;

    /// Write `data` to `remote_path` on the device with the given Unix file mode (e.g. 0o644).
    fn push(
        &self,
        data: &[u8],
        remote_path: &str,
        mode: u32,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Open a raw device service, e.g. `localabstract:scrcpy_0a1b2c3d`, `tcp:27183`, `tcpip:5555`.
    fn open(&self, service: &str) -> impl Future<Output = Result<Self::Stream>> + Send;
}
