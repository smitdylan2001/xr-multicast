//! Shell services: the shell protocol v2 framing and output collection.
//!
//! Shell v2 (`shell,v2,raw:<command>`, devices advertising `shell_v2`) frames
//! every chunk as `id: u8, length: u32 (little-endian), data`, separating
//! stdout, stderr and the exit code. Legacy `shell:<command>` streams merged
//! raw output and ends with the stream closing.

use bytes::{Buf, Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{AdbError, Result};

/// Shell v2 packet id: data for the process's stdin.
pub const ID_STDIN: u8 = 0;
/// Shell v2 packet id: process stdout.
pub const ID_STDOUT: u8 = 1;
/// Shell v2 packet id: process stderr.
pub const ID_STDERR: u8 = 2;
/// Shell v2 packet id: exit code (one byte of data).
pub const ID_EXIT: u8 = 3;
/// Shell v2 packet id: close the process's stdin.
pub const ID_CLOSE_STDIN: u8 = 4;
/// Shell v2 packet id: terminal window size change (PTY sessions).
pub const ID_WINDOW_SIZE_CHANGE: u8 = 5;

/// Size of a shell v2 packet header.
pub const V2_HEADER_LEN: usize = 5;

/// Service string for a command over shell protocol v2 without a PTY.
pub fn v2_service(command: &str) -> String {
    format!("shell,v2,raw:{command}")
}

/// Service string for a command over the legacy shell protocol.
pub fn legacy_service(command: &str) -> String {
    format!("shell:{command}")
}

/// Encode one shell v2 packet.
pub fn encode_v2_packet(id: u8, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(V2_HEADER_LEN + data.len());
    out.push(id);
    out.extend_from_slice(&u32::try_from(data.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(data);
    out
}

/// One decoded shell v2 packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V2Packet {
    /// Packet id (`ID_STDOUT`, `ID_STDERR`, `ID_EXIT`, ...).
    pub id: u8,
    /// Packet data.
    pub data: Bytes,
}

/// Incremental shell v2 decoder: feed bytes, take complete packets.
#[derive(Debug, Default)]
pub struct V2Decoder {
    buf: BytesMut,
}

impl V2Decoder {
    /// New, empty decoder.
    pub fn new() -> Self {
        V2Decoder::default()
    }

    /// Append received bytes.
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Take the next complete packet, if any.
    pub fn next_packet(&mut self) -> Option<V2Packet> {
        if self.buf.len() < V2_HEADER_LEN {
            return None;
        }
        let len = u32::from_le_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
        if self.buf.len() < V2_HEADER_LEN + len {
            return None;
        }
        let id = self.buf[0];
        self.buf.advance(V2_HEADER_LEN);
        let data = self.buf.split_to(len).freeze();
        Some(V2Packet { id, data })
    }

    /// Bytes buffered but not yet forming a complete packet.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }
}

/// Result of running a command to completion.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellOutput {
    /// Standard output (with legacy shell: stdout and stderr merged).
    pub stdout: Vec<u8>,
    /// Standard error (always empty with the legacy shell).
    pub stderr: Vec<u8>,
    /// Exit code (only known with shell v2).
    pub exit_code: Option<u8>,
}

impl ShellOutput {
    /// Stdout as (lossy) UTF-8.
    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Stderr as (lossy) UTF-8.
    pub fn stderr_str(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    /// Whether the command is known to have exited with code 0.
    pub fn success(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// Run the client side of a shell v2 session on an opened stream: close the
/// command's stdin, then collect stdout/stderr until the exit packet.
pub async fn collect_v2<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S) -> Result<ShellOutput> {
    // Nothing will be written to stdin: close it so commands reading it see EOF.
    stream
        .write_all(&encode_v2_packet(ID_CLOSE_STDIN, &[]))
        .await?;
    let mut decoder = V2Decoder::new();
    let mut out = ShellOutput::default();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        while let Some(packet) = decoder.next_packet() {
            match packet.id {
                ID_STDOUT => out.stdout.extend_from_slice(&packet.data),
                ID_STDERR => out.stderr.extend_from_slice(&packet.data),
                ID_EXIT => {
                    out.exit_code = packet.data.first().copied();
                    return Ok(out);
                }
                _ => {}
            }
        }
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            if decoder.pending() > 0 {
                return Err(AdbError::Protocol(
                    "shell v2 stream ended inside a packet".into(),
                ));
            }
            // Closed without an exit packet (e.g. the device killed the shell).
            return Ok(out);
        }
        decoder.push(&buf[..n]);
    }
}

/// Collect a legacy shell stream until the device closes it.
pub async fn collect_legacy<S: AsyncRead + Unpin>(stream: &mut S) -> Result<ShellOutput> {
    let mut out = ShellOutput::default();
    stream.read_to_end(&mut out.stdout).await?;
    Ok(out)
}

/// Shell-quote one argument for `sh -c` on the device (single quotes).
pub fn quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:,+@%".contains(&b))
    {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_and_decode_v2() {
        let mut stream = Vec::new();
        stream.extend(encode_v2_packet(ID_STDOUT, b"hello "));
        stream.extend(encode_v2_packet(ID_STDERR, b"oops"));
        stream.extend(encode_v2_packet(ID_STDOUT, b"world"));
        stream.extend(encode_v2_packet(ID_EXIT, &[3]));
        assert_eq!(&stream[..5], &[1, 6, 0, 0, 0]);

        // Feed byte by byte to exercise partial headers and payloads.
        let mut decoder = V2Decoder::new();
        let mut packets = Vec::new();
        for b in &stream {
            decoder.push(std::slice::from_ref(b));
            while let Some(p) = decoder.next_packet() {
                packets.push(p);
            }
        }
        assert_eq!(packets.len(), 4);
        assert_eq!(
            packets[0],
            V2Packet {
                id: ID_STDOUT,
                data: Bytes::from_static(b"hello ")
            }
        );
        assert_eq!(packets[1].id, ID_STDERR);
        assert_eq!(&packets[3].data[..], &[3]);
        assert_eq!(decoder.pending(), 0);
    }

    #[test]
    fn empty_packets() {
        let pkt = encode_v2_packet(ID_CLOSE_STDIN, &[]);
        assert_eq!(pkt, vec![4, 0, 0, 0, 0]);
        let mut decoder = V2Decoder::new();
        decoder.push(&pkt);
        assert_eq!(
            decoder.next_packet(),
            Some(V2Packet {
                id: ID_CLOSE_STDIN,
                data: Bytes::new()
            })
        );
        assert_eq!(decoder.next_packet(), None);
    }

    #[tokio::test]
    async fn collect_v2_over_duplex() {
        let (mut client, mut device) = tokio::io::duplex(64);
        let device_task = tokio::spawn(async move {
            let mut close = [0u8; 5];
            device.read_exact(&mut close).await.unwrap();
            assert_eq!(close, [ID_CLOSE_STDIN, 0, 0, 0, 0]);
            device
                .write_all(&encode_v2_packet(ID_STDOUT, &vec![b'x'; 1000]))
                .await
                .unwrap();
            device
                .write_all(&encode_v2_packet(ID_STDERR, b"err"))
                .await
                .unwrap();
            device
                .write_all(&encode_v2_packet(ID_EXIT, &[42]))
                .await
                .unwrap();
        });
        let out = collect_v2(&mut client).await.unwrap();
        device_task.await.unwrap();
        assert_eq!(out.stdout.len(), 1000);
        assert_eq!(out.stderr, b"err");
        assert_eq!(out.exit_code, Some(42));
        assert!(!out.success());
    }

    #[test]
    fn quoting() {
        assert_eq!(quote("ro.product.model"), "ro.product.model");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(quote(""), "''");
        assert_eq!(v2_service("ls"), "shell,v2,raw:ls");
        assert_eq!(legacy_service("ls"), "shell:ls");
    }
}
