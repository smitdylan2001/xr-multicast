//! scrcpy-server v5.0.1 client for XR MultiCast.
//!
//! * [`ServerOptions`]: the server's command line ([`ServerOptions::command`]).
//! * [`Demuxer`]: incremental, chunk-size independent parser for the video socket.
//! * [`parse_encoder_list`] / [`list_encoders`]: which encoders a device has.
//! * [`Session`]: push + start the server over any [`xrmc_adb::AdbDevice`],
//!   connect through the forward tunnel and stream [`SessionEvent`]s.
//!
//! Port of `src/main/stream-session.js` (and the server-launch parts of
//! `stream-manager.js`) from the Electron app.

mod demux;
mod encoders;
mod options;
mod session;

pub use demux::{
    DEFAULT_MAX_PACKET_SIZE, DEVICE_NAME_LENGTH, DemuxError, DemuxEvent, Demuxer, MediaPacket,
};
pub use encoders::{EncoderInfo, list_encoders, list_encoders_command, parse_encoder_list};
pub use options::{
    Crop, LogLevel, Orientation, Scid, ServerOptions, VideoCodec, codec_id_name, server_command,
    shell_quote,
};
pub use session::{
    Backpressure, Session, SessionConfig, SessionEvent, SessionHandle, SessionStats,
};

/// The scrcpy version this crate speaks; the server jar must be exactly this version.
pub const SCRCPY_VERSION: &str = "5.0.1";
/// Where the streaming server jar is pushed on the device.
pub const REMOTE_SERVER_PATH: &str = "/data/local/tmp/xrmc-scrcpy-server.jar";
/// Where the encoder-listing server jar is pushed (separate, because the
/// server deletes its own jar when it exits).
pub const REMOTE_LIST_SERVER_PATH: &str = "/data/local/tmp/xrmc-scrcpy-list.jar";
/// The server's main class.
pub const SERVER_CLASS: &str = "com.genymobile.scrcpy.Server";

/// Errors from scrcpy sessions and helpers.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// ADB transport error (push, shell, open).
    #[error(transparent)]
    Adb(#[from] xrmc_adb::AdbError),
    /// The server reported an error or exited. `message` is its last error
    /// log line (else its last log line).
    #[error("{message}")]
    Server {
        /// The most relevant log line.
        message: String,
        /// The last server log lines.
        log: Vec<String>,
    },
    /// The video socket could not be connected in time (server still running, no error logged).
    #[error("could not connect to the headset stream: {0}")]
    Connect(String),
    /// The video stream reported a fatal error.
    #[error(transparent)]
    Demux(#[from] DemuxError),
    /// The server streams a different codec than requested.
    #[error("Requested {requested} but the headset sent {received}")]
    CodecMismatch {
        /// What was asked for.
        requested: VideoCodec,
        /// What the stream says (`h264`, `vp9`, `0x…`).
        received: String,
    },
    /// The video stream closed without the server logging an error.
    #[error("Stream connection closed")]
    Closed,
    /// Socket I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// An operation took too long.
    #[error("timed out {0}")]
    Timeout(&'static str),
    /// `list_encoders` found no video encoders; holds the last output line.
    #[error("No encoders reported: {0}")]
    NoEncoders(String),
}
