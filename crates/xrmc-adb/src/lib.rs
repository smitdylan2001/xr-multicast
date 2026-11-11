//! Pure-Rust ADB client for XR MultiCast.
//!
//! Talks the ADB wire protocol directly over USB and TCP (no Google `adb`
//! binary needed), and can also go through a running adb server
//! (`localhost:5037`) so it coexists with other tools.
//!
//! Other crates program against the [`AdbDevice`] trait so they work the same
//! over any transport (and with test fakes).

use std::future::Future;

use tokio::io::{AsyncRead, AsyncWrite};

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
    fn push(&self, data: &[u8], remote_path: &str, mode: u32) -> impl Future<Output = Result<()>> + Send;

    /// Open a raw device service, e.g. `localabstract:scrcpy_0a1b2c3d`, `tcp:27183`, `tcpip:5555`.
    fn open(&self, service: &str) -> impl Future<Output = Result<Self::Stream>> + Send;
}
