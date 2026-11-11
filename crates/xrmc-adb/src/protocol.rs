//! ADB wire protocol: the 24-byte message header, commands and constants.
//!
//! Every ADB message is a little-endian header
//! `command, arg0, arg1, data_length, data_check, magic` followed by
//! `data_length` bytes of payload. `magic` is `command ^ 0xffff_ffff`, and
//! `data_check` is the byte sum of the payload for protocol versions before
//! [`A_VERSION_SKIP_CHECKSUM`] (zero afterwards).

use bytes::Bytes;

use crate::{AdbError, Result};

/// Connect: `CNXN(version, max_payload, "system-identity-string")`.
pub const A_CNXN: u32 = 0x4e58_4e43;
/// Authentication: `AUTH(type, 0, data)`.
pub const A_AUTH: u32 = 0x4854_5541;
/// Open a stream: `OPEN(local_id, 0, "destination\0")`.
pub const A_OPEN: u32 = 0x4e45_504f;
/// Ready / acknowledge: `OKAY(local_id, remote_id, "")`.
pub const A_OKAY: u32 = 0x5941_4b4f;
/// Close a stream: `CLSE(local_id, remote_id, "")`.
pub const A_CLSE: u32 = 0x4553_4c43;
/// Write stream data: `WRTE(local_id, remote_id, data)`.
pub const A_WRTE: u32 = 0x4554_5257;
/// Start TLS: `STLS(version, 0, "")` (Android 11+ wireless debugging).
pub const A_STLS: u32 = 0x534c_5453;
/// Obsolete sync command, never sent by modern devices.
pub const A_SYNC: u32 = 0x434e_5953;

/// The protocol version spoken before checksums were dropped.
pub const A_VERSION_MIN: u32 = 0x0100_0000;
/// First protocol version that no longer requires `data_check`.
pub const A_VERSION_SKIP_CHECKSUM: u32 = 0x0100_0001;
/// The protocol version this client announces.
pub const A_VERSION: u32 = 0x0100_0001;

/// Maximum payload of protocol version 1 devices (`A_VERSION_MIN`).
pub const MAX_PAYLOAD_V1: u32 = 4 * 1024;
/// Maximum payload this client announces and accepts (1 MiB, like adb).
pub const MAX_PAYLOAD: u32 = 1024 * 1024;

/// `AUTH` type: the device sends a random token to sign.
pub const AUTH_TOKEN: u32 = 1;
/// `AUTH` type: the host replies with a signature of the token.
pub const AUTH_SIGNATURE: u32 = 2;
/// `AUTH` type: the host offers its public key (shows "Allow USB debugging").
pub const AUTH_RSAPUBLICKEY: u32 = 3;

/// Size of the random token in `AUTH(TOKEN)`.
pub const TOKEN_SIZE: usize = 20;

/// Version announced in `STLS` messages.
pub const A_STLS_VERSION: u32 = 0x0100_0000;

/// Size of the message header in bytes.
pub const HEADER_LEN: usize = 24;

/// Byte sum of `data`, as used by the legacy `data_check` field.
pub fn checksum(data: &[u8]) -> u32 {
    data.iter()
        .fold(0u32, |sum, &b| sum.wrapping_add(u32::from(b)))
}

/// Human-readable name of an ADB command (for logs and errors).
pub fn command_name(command: u32) -> String {
    match command {
        A_CNXN => "CNXN".into(),
        A_AUTH => "AUTH".into(),
        A_OPEN => "OPEN".into(),
        A_OKAY => "OKAY".into(),
        A_CLSE => "CLSE".into(),
        A_WRTE => "WRTE".into(),
        A_STLS => "STLS".into(),
        A_SYNC => "SYNC".into(),
        other => format!("0x{other:08x}"),
    }
}

/// A decoded 24-byte message header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Command (`A_CNXN`, `A_OPEN`, ...).
    pub command: u32,
    /// First argument.
    pub arg0: u32,
    /// Second argument.
    pub arg1: u32,
    /// Length of the payload that follows.
    pub data_length: u32,
    /// Byte sum of the payload (only meaningful for old protocol versions).
    pub data_check: u32,
    /// `command ^ 0xffff_ffff`.
    pub magic: u32,
}

impl Header {
    /// Decode and validate a header (magic must match the command).
    pub fn decode(bytes: &[u8; HEADER_LEN]) -> Result<Header> {
        let word =
            |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
        let header = Header {
            command: word(0),
            arg0: word(4),
            arg1: word(8),
            data_length: word(12),
            data_check: word(16),
            magic: word(20),
        };
        if header.magic != header.command ^ 0xffff_ffff {
            return Err(AdbError::Protocol(format!(
                "bad message magic 0x{:08x} for command 0x{:08x}",
                header.magic, header.command
            )));
        }
        Ok(header)
    }

    /// Encode the header into its 24-byte wire form.
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        for (i, word) in [
            self.command,
            self.arg0,
            self.arg1,
            self.data_length,
            self.data_check,
            self.magic,
        ]
        .into_iter()
        .enumerate()
        {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        out
    }
}

/// One ADB message: a command, two arguments and a payload.
#[derive(Clone, PartialEq, Eq)]
pub struct Message {
    /// Command (`A_CNXN`, `A_OPEN`, ...).
    pub command: u32,
    /// First argument.
    pub arg0: u32,
    /// Second argument.
    pub arg1: u32,
    /// Payload (may be empty).
    pub payload: Bytes,
}

impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("Message");
        s.field("command", &command_name(self.command))
            .field("arg0", &self.arg0)
            .field("arg1", &self.arg1)
            .field("len", &self.payload.len());
        if self.command != A_WRTE && self.command != A_AUTH {
            s.field(
                "payload",
                &String::from_utf8_lossy(&self.payload[..self.payload.len().min(96)]),
            );
        }
        s.finish()
    }
}

impl Message {
    /// Build a message.
    pub fn new(command: u32, arg0: u32, arg1: u32, payload: impl Into<Bytes>) -> Message {
        Message {
            command,
            arg0,
            arg1,
            payload: payload.into(),
        }
    }

    /// Header for this message. `with_checksum` fills in `data_check`
    /// (required before [`A_VERSION_SKIP_CHECKSUM`]).
    pub fn header(&self, with_checksum: bool) -> Header {
        Header {
            command: self.command,
            arg0: self.arg0,
            arg1: self.arg1,
            data_length: u32::try_from(self.payload.len()).unwrap_or(u32::MAX),
            data_check: if with_checksum {
                checksum(&self.payload)
            } else {
                0
            },
            magic: self.command ^ 0xffff_ffff,
        }
    }

    /// Header plus payload in one buffer (handy for stream transports and tests).
    pub fn encode(&self, with_checksum: bool) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        out.extend_from_slice(&self.header(with_checksum).encode());
        out.extend_from_slice(&self.payload);
        out
    }

    /// Combine a decoded header with its payload, checking the length and,
    /// if `verify_checksum` is set, the legacy byte-sum checksum.
    pub fn from_parts(header: Header, payload: Bytes, verify_checksum: bool) -> Result<Message> {
        if payload.len() != header.data_length as usize {
            return Err(AdbError::Protocol(format!(
                "payload length {} does not match header length {}",
                payload.len(),
                header.data_length
            )));
        }
        if verify_checksum && header.data_check != checksum(&payload) {
            return Err(AdbError::Protocol(format!(
                "bad checksum on {} message",
                command_name(header.command)
            )));
        }
        Ok(Message {
            command: header.command,
            arg0: header.arg0,
            arg1: header.arg1,
            payload,
        })
    }
}

/// Features this client announces in its `CNXN` banner.
///
/// `delayed_ack` is intentionally absent: streams use the classic
/// one-`OKAY`-per-`WRTE` flow control.
pub const HOST_FEATURES: &[&str] = &[
    "shell_v2",
    "cmd",
    "stat_v2",
    "ls_v2",
    "fixed_push_mkdir",
    "apex",
    "abb",
    "fixed_push_symlink_timestamp",
    "abb_exec",
    "remount_shell",
    "track_app",
    "sendrecv_v2",
    "openscreen_mdns",
];

/// The `CNXN` payload sent by this client: `host::features=...`.
pub fn host_banner() -> String {
    format!("host::features={}", HOST_FEATURES.join(","))
}

/// The parsed identity string a device sends in its `CNXN` message, e.g.
/// `device::ro.product.name=hollywood;ro.product.model=Quest 3;ro.product.device=eureka;features=shell_v2,cmd,...`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceBanner {
    /// System type before `::` (`device`, `recovery`, `sideload`, `bootloader`, `rescue`, `host`).
    pub kind: String,
    /// Key/value properties (`ro.product.name`, `ro.product.model`, `ro.product.device`, ...).
    pub props: std::collections::BTreeMap<String, String>,
    /// Features the device supports (`shell_v2`, `cmd`, `sendrecv_v2`, ...).
    pub features: Vec<String>,
    /// The raw banner text.
    pub raw: String,
}

impl DeviceBanner {
    /// Parse a `CNXN` identity string. Unknown or malformed parts are ignored.
    pub fn parse(raw: &[u8]) -> DeviceBanner {
        let text = String::from_utf8_lossy(raw);
        let text = text.trim_end_matches('\0').to_string();
        let (kind, rest) = match text.split_once(':') {
            Some((kind, rest)) => (kind.to_string(), rest.strip_prefix(':').unwrap_or(rest)),
            None => (text.clone(), ""),
        };
        let mut banner = DeviceBanner {
            kind,
            raw: text.clone(),
            ..DeviceBanner::default()
        };
        for part in rest.split(';').filter(|p| !p.is_empty()) {
            let Some((key, value)) = part.split_once('=') else {
                continue;
            };
            if key == "features" {
                banner.features = value
                    .split(',')
                    .filter(|f| !f.is_empty())
                    .map(str::to_string)
                    .collect();
            } else {
                banner.props.insert(key.to_string(), value.to_string());
            }
        }
        banner
    }

    /// Whether the device advertises `feature`.
    pub fn has_feature(&self, feature: &str) -> bool {
        self.features.iter().any(|f| f == feature)
    }

    /// `ro.product.model` from the banner, if present.
    pub fn model(&self) -> Option<&str> {
        self.props.get("ro.product.model").map(String::as_str)
    }

    /// `ro.product.name` from the banner, if present.
    pub fn product(&self) -> Option<&str> {
        self.props.get("ro.product.name").map(String::as_str)
    }

    /// `ro.product.device` from the banner, if present.
    pub fn device(&self) -> Option<&str> {
        self.props.get("ro.product.device").map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_constants_are_ascii() {
        assert_eq!(&A_CNXN.to_le_bytes(), b"CNXN");
        assert_eq!(&A_AUTH.to_le_bytes(), b"AUTH");
        assert_eq!(&A_OPEN.to_le_bytes(), b"OPEN");
        assert_eq!(&A_OKAY.to_le_bytes(), b"OKAY");
        assert_eq!(&A_CLSE.to_le_bytes(), b"CLSE");
        assert_eq!(&A_WRTE.to_le_bytes(), b"WRTE");
        assert_eq!(&A_STLS.to_le_bytes(), b"STLS");
        assert_eq!(&A_SYNC.to_le_bytes(), b"SYNC");
    }

    #[test]
    fn header_round_trip() {
        let msg = Message::new(A_WRTE, 7, 0x1234_5678, &b"hello"[..]);
        let header = msg.header(true);
        let bytes = header.encode();
        assert_eq!(&bytes[0..4], b"WRTE");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 7);
        assert_eq!(
            u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            0x1234_5678
        );
        assert_eq!(u32::from_le_bytes(bytes[12..16].try_into().unwrap()), 5);
        assert_eq!(
            u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
            checksum(b"hello")
        );
        assert_eq!(
            u32::from_le_bytes(bytes[20..24].try_into().unwrap()),
            A_WRTE ^ 0xffff_ffff
        );
        let decoded = Header::decode(&bytes).unwrap();
        assert_eq!(decoded, header);
        let back = Message::from_parts(decoded, msg.payload.clone(), true).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn known_cnxn_header_bytes() {
        // A CNXN header as produced by adb for a 7-byte "host::\0" payload with checksum.
        let msg = Message::new(A_CNXN, A_VERSION, MAX_PAYLOAD, &b"host::\0"[..]);
        let bytes = msg.header(true).encode();
        let expected: [u8; 24] = [
            0x43, 0x4e, 0x58, 0x4e, // CNXN
            0x01, 0x00, 0x00, 0x01, // version 0x01000001
            0x00, 0x00, 0x10, 0x00, // max payload 1 MiB
            0x07, 0x00, 0x00, 0x00, // length
            0x32, 0x02, 0x00, 0x00, // checksum: sum("host::\0") = 0x232
            0xbc, 0xb1, 0xa7, 0xb1, // magic
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn checksum_is_byte_sum() {
        assert_eq!(checksum(b""), 0);
        assert_eq!(checksum(&[0xff; 4]), 0x3fc);
        assert_eq!(checksum(b"host::"), 0x232);
        // Wraps instead of overflowing.
        let big = vec![0xffu8; 16_843_010];
        assert_eq!(checksum(&big), (16_843_010u64 * 255 % (1u64 << 32)) as u32);
    }

    #[test]
    fn bad_magic_and_checksum_are_rejected() {
        let msg = Message::new(A_OKAY, 1, 2, Bytes::new());
        let mut bytes = msg.header(false).encode();
        bytes[20] ^= 1;
        assert!(Header::decode(&bytes).is_err());

        let msg = Message::new(A_WRTE, 1, 2, &b"abc"[..]);
        let mut header = msg.header(true);
        header.data_check += 1;
        assert!(Message::from_parts(header, msg.payload.clone(), true).is_err());
        // Without verification (modern protocol) the checksum is ignored.
        assert!(Message::from_parts(header, msg.payload.clone(), false).is_ok());
        // Length mismatch is always an error.
        assert!(Message::from_parts(header, Bytes::from_static(b"ab"), false).is_err());
    }

    #[test]
    fn banner_parsing() {
        let b = DeviceBanner::parse(
            b"device::ro.product.name=hollywood;ro.product.model=Quest 3;ro.product.device=eureka;features=shell_v2,cmd,stat_v2\0",
        );
        assert_eq!(b.kind, "device");
        assert_eq!(b.model(), Some("Quest 3"));
        assert_eq!(b.product(), Some("hollywood"));
        assert_eq!(b.device(), Some("eureka"));
        assert!(b.has_feature("shell_v2"));
        assert!(b.has_feature("cmd"));
        assert!(!b.has_feature("abb"));

        let legacy = DeviceBanner::parse(b"device::");
        assert_eq!(legacy.kind, "device");
        assert!(legacy.features.is_empty());
        assert!(host_banner().starts_with("host::features=shell_v2,"));
        assert!(!host_banner().contains("delayed_ack"));
    }
}
