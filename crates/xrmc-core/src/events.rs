//! Events the core sends to its UI(s) and the remote API.

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::device_manager::DeviceSnapshot;
use crate::stream_manager::StreamState;

/// Severity of a [`Notice`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NoticeLevel {
    Ok,
    Info,
    Warn,
    Error,
}

/// A short message for the user (`{ level, message }`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notice {
    pub level: NoticeLevel,
    pub message: String,
}

impl Notice {
    pub fn new(level: NoticeLevel, message: impl Into<String>) -> Self {
        Notice {
            level,
            message: message.into(),
        }
    }
}

/// One demuxed video packet from a headset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoPacket {
    /// Codec configuration (SPS/PPS/VPS or AV1 sequence header), not a frame.
    pub config: bool,
    /// Key frame.
    pub key: bool,
    /// Presentation timestamp in microseconds.
    pub pts: u64,
    pub data: Bytes,
}

/// Everything the core reports, on one broadcast channel.
///
/// Receivers that fall behind get [`broadcast::error::RecvError::Lagged`]; they
/// should then re-sync from `DeviceManager::snapshot()` / `StreamManager::states()`
/// and wait for the next key frame.
#[derive(Debug, Clone)]
pub enum CoreEvent {
    /// The device list changed.
    DevicesChanged(DeviceSnapshot),
    /// A stream changed state.
    StreamState(StreamState),
    /// A video packet of stream `id` (a headset's hardware serial).
    Packet { id: String, packet: VideoPacket },
    /// A message for the user.
    Notice(Notice),
    /// A log line from a stream's scrcpy-server.
    StreamLog { id: String, line: String },
    /// A verified headset connected while "cast on connect" is on: the host should
    /// call `StreamManager::start(id, ..)`.
    AutoCast { id: String },
}

impl CoreEvent {
    /// The remote-API envelope (`{ "t": "event", "event": ..., "args": [...] }`) for
    /// events that are sent as JSON. Packets travel as binary frames and logs stay local.
    pub fn to_remote_json(&self) -> Option<Value> {
        let (event, arg) = match self {
            CoreEvent::DevicesChanged(s) => ("devices", serde_json::to_value(s).ok()?),
            CoreEvent::StreamState(s) => ("state", serde_json::to_value(s).ok()?),
            CoreEvent::Notice(n) => ("notice", serde_json::to_value(n).ok()?),
            CoreEvent::Packet { .. } | CoreEvent::StreamLog { .. } | CoreEvent::AutoCast { .. } => {
                return None;
            }
        };
        Some(json!({ "t": "event", "event": event, "args": [arg] }))
    }
}

/// Sender side of the event channel, shared by the managers.
pub type EventSender = broadcast::Sender<CoreEvent>;

/// A new event channel. Packets go through it too, so size it for a few
/// seconds of video across all headsets (e.g. 1024).
pub fn channel(capacity: usize) -> (EventSender, broadcast::Receiver<CoreEvent>) {
    broadcast::channel(capacity.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices_serialise_like_the_electron_app() {
        let n = Notice::new(NoticeLevel::Warn, "careful");
        assert_eq!(
            serde_json::to_value(&n).unwrap(),
            json!({ "level": "warn", "message": "careful" })
        );
        let e = CoreEvent::Notice(n).to_remote_json().unwrap();
        assert_eq!(
            e,
            json!({ "t": "event", "event": "notice", "args": [{ "level": "warn", "message": "careful" }] })
        );
        let p = CoreEvent::Packet {
            id: "S".into(),
            packet: VideoPacket {
                config: false,
                key: true,
                pts: 0,
                data: Bytes::from_static(b"x"),
            },
        };
        assert!(p.to_remote_json().is_none());
    }
}
