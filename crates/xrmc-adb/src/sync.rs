//! The file sync service (`sync:`), used for `push`.
//!
//! Requests are `id: [u8; 4], length: u32 (little-endian)` followed by
//! `length` bytes. A push is `SEND "path,mode"`, then `DATA` chunks of at most
//! 64 KiB, then `DONE mtime`; the device answers `OKAY` or `FAIL message`.

use std::time::{SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{AdbError, Result};

/// Largest `DATA` chunk the sync protocol allows.
pub const SYNC_DATA_MAX: usize = 64 * 1024;
/// Longest remote path adbd accepts.
pub const SYNC_PATH_MAX: usize = 1024;

/// Sync request/response ids.
pub mod id {
    /// Start a push: `SEND "path,mode"`.
    pub const SEND: [u8; 4] = *b"SEND";
    /// A chunk of file data.
    pub const DATA: [u8; 4] = *b"DATA";
    /// End of a push; the length field carries the mtime.
    pub const DONE: [u8; 4] = *b"DONE";
    /// Success.
    pub const OKAY: [u8; 4] = *b"OKAY";
    /// Failure with a message.
    pub const FAIL: [u8; 4] = *b"FAIL";
    /// Leave the sync service.
    pub const QUIT: [u8; 4] = *b"QUIT";
    /// `lstat` a path (v1).
    pub const STAT: [u8; 4] = *b"STAT";
}

/// Encode a sync request header plus data.
pub fn request(id: [u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + data.len());
    out.extend_from_slice(&id);
    out.extend_from_slice(&u32::try_from(data.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(data);
    out
}

/// Encode a sync request whose length field carries a value instead (`DONE mtime`, `QUIT 0`).
pub fn request_with_value(id: [u8; 4], value: u32) -> [u8; 8] {
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&id);
    out[4..].copy_from_slice(&value.to_le_bytes());
    out
}

/// The `SEND` request for `remote_path` with Unix `mode` (permission bits;
/// the regular-file type bit is added like adb does).
pub fn send_request(remote_path: &str, mode: u32) -> Result<Vec<u8>> {
    if remote_path.len() > SYNC_PATH_MAX {
        return Err(AdbError::Other(format!(
            "remote path longer than {SYNC_PATH_MAX} bytes"
        )));
    }
    if remote_path.is_empty() || remote_path.contains('\0') {
        return Err(AdbError::Other(format!(
            "invalid remote path {remote_path:?}"
        )));
    }
    const S_IFREG: u32 = 0o100_000;
    let spec = format!("{remote_path},{}", (mode & 0o7777) | S_IFREG);
    Ok(request(id::SEND, spec.as_bytes()))
}

/// File metadata returned by [`stat`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStat {
    /// `st_mode` (type and permission bits); 0 if the path does not exist.
    pub mode: u32,
    /// File size (truncated to 32 bits by the v1 protocol).
    pub size: u32,
    /// Modification time (seconds since the epoch).
    pub mtime: u32,
}

impl FileStat {
    /// Whether the path exists.
    pub fn exists(&self) -> bool {
        self.mode != 0
    }
}

fn now_secs() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u32::try_from(d.as_secs()).unwrap_or(u32::MAX))
        .unwrap_or(0)
}

/// Push `data` to `remote_path` over an opened `sync:` stream, then send `QUIT`.
///
/// Note that adbd copies the owner permission bits to group and other
/// (`0o640` becomes `0o666`, `0o755` becomes `0o777`), like `adb push`.
///
/// `max_write` is the size of each write to the stream (use the connection's
/// max payload so several `DATA` chunks travel in one ADB message).
pub async fn push<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    data: &[u8],
    remote_path: &str,
    mode: u32,
    max_write: usize,
) -> Result<()> {
    let max_write = max_write.max(SYNC_DATA_MAX + 8);
    let mut batch = send_request(remote_path, mode)?;
    let mut write_result: std::io::Result<()> = Ok(());
    for chunk in data.chunks(SYNC_DATA_MAX) {
        if batch.len() + 8 + chunk.len() > max_write {
            write_result = stream.write_all(&batch).await;
            batch.clear();
            if write_result.is_err() {
                break;
            }
        }
        batch.extend_from_slice(&id::DATA);
        batch.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
        batch.extend_from_slice(chunk);
    }
    if write_result.is_ok() {
        batch.extend_from_slice(&request_with_value(id::DONE, now_secs()));
        write_result = stream.write_all(&batch).await;
    }
    if write_result.is_ok() {
        write_result = stream.flush().await;
    }
    // On failure adbd sends FAIL and closes the stream, which makes our writes
    // fail: prefer its message over the write error.
    let status = read_status(stream).await;
    match (status, write_result) {
        (Ok(()), Ok(())) => {
            let _ = stream.write_all(&request_with_value(id::QUIT, 0)).await;
            let _ = stream.flush().await;
            Ok(())
        }
        (Err(AdbError::Other(msg)), _) => Err(AdbError::Other(format!(
            "push to {remote_path} failed: {msg}"
        ))),
        (Ok(()), Err(e)) | (Err(_), Err(e)) => Err(AdbError::Io(e)),
        (Err(e), Ok(())) => Err(e),
    }
}

/// Read a sync `OKAY` (→ `Ok`) or `FAIL message` (→ `AdbError::Other(message)`).
pub async fn read_status<S: AsyncRead + Unpin>(stream: &mut S) -> Result<()> {
    let mut header = [0u8; 8];
    read_exact(stream, &mut header).await?;
    let len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
    match [header[0], header[1], header[2], header[3]] {
        id::OKAY => Ok(()),
        id::FAIL => {
            let mut msg = vec![0u8; len.min(64 * 1024)];
            read_exact(stream, &mut msg).await?;
            Err(AdbError::Other(String::from_utf8_lossy(&msg).into_owned()))
        }
        other => Err(AdbError::Protocol(format!(
            "unexpected sync response {:?}",
            String::from_utf8_lossy(&other)
        ))),
    }
}

/// `lstat` a remote path over an opened `sync:` stream (sync v1 `STAT`).
pub async fn stat<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    remote_path: &str,
) -> Result<FileStat> {
    if remote_path.len() > SYNC_PATH_MAX {
        return Err(AdbError::Other(format!(
            "remote path longer than {SYNC_PATH_MAX} bytes"
        )));
    }
    stream
        .write_all(&request(id::STAT, remote_path.as_bytes()))
        .await?;
    stream.flush().await?;
    let mut reply = [0u8; 16];
    read_exact(stream, &mut reply).await?;
    if reply[..4] != id::STAT {
        return Err(AdbError::Protocol("unexpected reply to STAT".into()));
    }
    let word = |i: usize| u32::from_le_bytes([reply[i], reply[i + 1], reply[i + 2], reply[i + 3]]);
    Ok(FileStat {
        mode: word(4),
        size: word(8),
        mtime: word(12),
    })
}

async fn read_exact<S: AsyncRead + Unpin>(stream: &mut S, buf: &mut [u8]) -> Result<()> {
    match stream.read_exact(buf).await {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            Err(AdbError::Protocol("sync stream closed unexpectedly".into()))
        }
        Err(e) => Err(AdbError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing() {
        assert_eq!(request(id::DATA, b"abc"), b"DATA\x03\x00\x00\x00abc");
        assert_eq!(
            request_with_value(id::DONE, 0x0102_0304),
            *b"DONE\x04\x03\x02\x01"
        );
        // 0o644 | S_IFREG = 0o100644 = 33188.
        assert_eq!(
            send_request("/data/local/tmp/x", 0o644).unwrap(),
            request(id::SEND, b"/data/local/tmp/x,33188")
        );
        assert_eq!(
            send_request("/x", 0o755).unwrap(),
            request(id::SEND, b"/x,33261")
        );
        assert!(send_request("", 0o644).is_err());
        assert!(send_request(&"a".repeat(2000), 0o644).is_err());
    }

    /// Parse a push session the way adbd does and return (spec, data, mtime).
    async fn fake_sync_server<S: AsyncRead + AsyncWrite + Unpin>(
        s: &mut S,
        fail: Option<&str>,
    ) -> (String, Vec<u8>, u32) {
        let mut header = [0u8; 8];
        s.read_exact(&mut header).await.unwrap();
        assert_eq!(&header[..4], b"SEND");
        let len = u32::from_le_bytes(header[4..].try_into().unwrap()) as usize;
        let mut spec = vec![0u8; len];
        s.read_exact(&mut spec).await.unwrap();
        let mut data = Vec::new();
        loop {
            s.read_exact(&mut header).await.unwrap();
            let len = u32::from_le_bytes(header[4..].try_into().unwrap());
            match &header[..4] {
                b"DATA" => {
                    assert!(len as usize <= SYNC_DATA_MAX);
                    let mut chunk = vec![0u8; len as usize];
                    s.read_exact(&mut chunk).await.unwrap();
                    data.extend(chunk);
                }
                b"DONE" => {
                    match fail {
                        Some(msg) => s
                            .write_all(&request(id::FAIL, msg.as_bytes()))
                            .await
                            .unwrap(),
                        None => s.write_all(&request_with_value(id::OKAY, 0)).await.unwrap(),
                    }
                    if fail.is_none() {
                        s.read_exact(&mut header).await.unwrap();
                        assert_eq!(&header[..4], b"QUIT");
                    }
                    return (String::from_utf8(spec).unwrap(), data, len);
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn push_over_duplex() {
        let (mut client, mut device) = tokio::io::duplex(1 << 16);
        let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let expected = payload.clone();
        let server = tokio::spawn(async move { fake_sync_server(&mut device, None).await });
        push(&mut client, &payload, "/data/local/tmp/f", 0o600, 1 << 20)
            .await
            .unwrap();
        let (spec, data, mtime) = server.await.unwrap();
        assert_eq!(spec, "/data/local/tmp/f,33152");
        assert_eq!(data, expected);
        assert!(mtime > 1_600_000_000);
    }

    #[tokio::test]
    async fn push_failure_reports_device_message() {
        let (mut client, mut device) = tokio::io::duplex(1 << 16);
        let server =
            tokio::spawn(
                async move { fake_sync_server(&mut device, Some("Permission denied")).await },
            );
        let err = push(&mut client, b"x", "/system/f", 0o644, 4096)
            .await
            .unwrap_err();
        server.await.unwrap();
        assert!(err.to_string().contains("Permission denied"), "{err}");
    }
}
