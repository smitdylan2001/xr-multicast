//! Message-level transports: anything that can carry ADB messages.
//!
//! A transport is split into a [`MessageReader`] and a [`MessageWriter`] half
//! so the connection can read and write concurrently. Byte-stream transports
//! (TCP, in-memory pipes) use [`StreamReader`]/[`StreamWriter`]; USB has its
//! own implementation in [`crate::usb`] because bulk endpoints need
//! packet-aware reads and zero-length packets.

use std::future::Future;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpStream;

use crate::protocol::{HEADER_LEN, Header, MAX_PAYLOAD, Message};
use crate::{AdbError, Result};

/// Reads whole ADB messages.
pub trait MessageReader: Send + 'static {
    /// Read the next message. `verify_checksum` enables the legacy
    /// `data_check` verification (protocol versions before `A_VERSION_SKIP_CHECKSUM`).
    fn read_message(
        &mut self,
        verify_checksum: bool,
    ) -> impl Future<Output = Result<Message>> + Send;
}

/// Writes whole ADB messages.
pub trait MessageWriter: Send + 'static {
    /// Queue or send one message. `with_checksum` fills in the legacy `data_check` field.
    fn write_message(
        &mut self,
        msg: &Message,
        with_checksum: bool,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Push out anything buffered by [`MessageWriter::write_message`].
    fn flush(&mut self) -> impl Future<Output = Result<()>> + Send;
}

/// Reads messages from a byte stream (header, then payload).
pub struct StreamReader<R> {
    inner: BufReader<R>,
}

impl<R: AsyncRead + Unpin + Send + 'static> StreamReader<R> {
    /// Wrap a byte stream.
    pub fn new(inner: R) -> Self {
        StreamReader {
            inner: BufReader::with_capacity(64 * 1024, inner),
        }
    }
}

impl<R: AsyncRead + Unpin + Send + 'static> MessageReader for StreamReader<R> {
    async fn read_message(&mut self, verify_checksum: bool) -> Result<Message> {
        let mut raw = [0u8; HEADER_LEN];
        read_exact_or_disconnect(&mut self.inner, &mut raw).await?;
        let header = Header::decode(&raw)?;
        check_length(&header)?;
        let mut payload = BytesMut::zeroed(header.data_length as usize);
        if !payload.is_empty() {
            read_exact_or_disconnect(&mut self.inner, &mut payload).await?;
        }
        Message::from_parts(header, payload.freeze(), verify_checksum)
    }
}

/// Writes messages to a byte stream.
pub struct StreamWriter<W: AsyncWrite> {
    inner: BufWriter<W>,
}

impl<W: AsyncWrite + Unpin + Send + 'static> StreamWriter<W> {
    /// Wrap a byte stream.
    pub fn new(inner: W) -> Self {
        StreamWriter {
            inner: BufWriter::with_capacity(64 * 1024, inner),
        }
    }
}

impl<W: AsyncWrite + Unpin + Send + 'static> MessageWriter for StreamWriter<W> {
    async fn write_message(&mut self, msg: &Message, with_checksum: bool) -> Result<()> {
        self.inner
            .write_all(&msg.header(with_checksum).encode())
            .await?;
        self.inner.write_all(&msg.payload).await?;
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        self.inner.flush().await?;
        Ok(())
    }
}

/// Split any bidirectional byte stream into message reader/writer halves.
pub fn split_stream<S>(
    stream: S,
) -> (
    StreamReader<tokio::io::ReadHalf<S>>,
    StreamWriter<tokio::io::WriteHalf<S>>,
)
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (r, w) = tokio::io::split(stream);
    (StreamReader::new(r), StreamWriter::new(w))
}

/// TCP transport halves.
pub type TcpTransport = (
    StreamReader<tokio::net::tcp::OwnedReadHalf>,
    StreamWriter<tokio::net::tcp::OwnedWriteHalf>,
);

/// Connect to an adbd listening on TCP (`adb tcpip` mode, emulators) and
/// return the transport halves.
pub async fn connect_tcp(addr: &str, timeout: Duration) -> Result<TcpTransport> {
    let stream = match tokio::time::timeout(timeout, TcpStream::connect(addr)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            return Err(AdbError::Io(std::io::Error::new(
                e.kind(),
                format!("could not connect to {addr}: {e}"),
            )));
        }
        Err(_) => return Err(AdbError::Timeout),
    };
    stream.set_nodelay(true)?;
    let (r, w) = stream.into_split();
    Ok((StreamReader::new(r), StreamWriter::new(w)))
}

pub(crate) fn check_length(header: &Header) -> Result<()> {
    if header.data_length > MAX_PAYLOAD {
        return Err(AdbError::Protocol(format!(
            "message payload of {} bytes exceeds the {MAX_PAYLOAD}-byte maximum",
            header.data_length
        )));
    }
    Ok(())
}

async fn read_exact_or_disconnect<R: AsyncRead + Unpin>(r: &mut R, buf: &mut [u8]) -> Result<()> {
    match r.read_exact(buf).await {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(AdbError::Disconnected),
        Err(e) => Err(AdbError::Io(e)),
    }
}

/// Convenience used by tests and the fake device: read one message from a raw stream.
pub async fn read_message_from<R: AsyncRead + Unpin>(
    r: &mut R,
    verify_checksum: bool,
) -> Result<Message> {
    let mut raw = [0u8; HEADER_LEN];
    read_exact_or_disconnect(r, &mut raw).await?;
    let header = Header::decode(&raw)?;
    check_length(&header)?;
    let mut payload = vec![0u8; header.data_length as usize];
    read_exact_or_disconnect(r, &mut payload).await?;
    Message::from_parts(header, Bytes::from(payload), verify_checksum)
}
