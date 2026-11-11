//! Direct USB transport (no adb server) built on `nusb`.
//!
//! ADB devices expose a vendor-specific interface (class `0xff`, subclass
//! `0x42`, protocol `0x01`) with one bulk IN and one bulk OUT endpoint. Each
//! ADB message is sent as two bulk transfers (header, then payload), each
//! followed by a zero-length packet when its length is a multiple of the
//! endpoint's max packet size (like adb's `zero_mask`). Reads request exactly
//! the bytes still needed (rounded up to the packet size), so a device that
//! does not send zero-length packets never leaves a read hanging.
//!
//! Platform notes:
//! * **Linux**: needs write access to `/dev/bus/usb/BBB/DDD` (udev rule). A
//!   running adb server claims every ADB interface, which makes [`UsbTransport::open`]
//!   fail with [`std::io::ErrorKind::ResourceBusy`]; use the adb-server
//!   transport then (see [`crate::AdbHost::connect_usb_or_server`]).
//! * **Android**: there is no enumeration here; get a file descriptor from
//!   `UsbManager.openDevice(device).getFileDescriptor()` and use
//!   [`UsbTransport::from_fd`].
//! * **Windows**: the ADB interface must be bound to WinUSB (Google USB
//!   driver / the headset vendor's ADB driver are WinUSB based). A device
//!   held by a running adb server reports access denied / busy.
//! * **macOS**: works out of the box; the adb server also holds devices exclusively.

use bytes::{Bytes, BytesMut};
use nusb::transfer::{Buffer, Bulk, Direction, In, Out, TransferError};
use nusb::{Endpoint, Interface};

use crate::protocol::{HEADER_LEN, Header, Message};
use crate::transport::{MessageReader, MessageWriter, check_length};
use crate::{AdbError, Result};

/// USB interface class of the ADB interface.
pub const ADB_CLASS: u8 = 0xff;
/// USB interface subclass of the ADB interface.
pub const ADB_SUBCLASS: u8 = 0x42;
/// USB interface protocol of the ADB interface.
pub const ADB_PROTOCOL: u8 = 0x01;

/// Largest single IN transfer we submit.
const MAX_IN_TRANSFER: usize = 256 * 1024;

/// A USB device that exposes an ADB interface, found without opening it.
#[derive(Debug, Clone)]
pub struct UsbDeviceInfo {
    /// USB vendor id (e.g. `0x2833` Meta/Oculus).
    pub vendor_id: u16,
    /// USB product id.
    pub product_id: u16,
    /// iSerialNumber string (this is what adb shows as the device serial).
    pub serial: Option<String>,
    /// iManufacturer string.
    pub manufacturer: Option<String>,
    /// iProduct string.
    pub product: Option<String>,
    /// Bus / host controller identifier.
    pub bus_id: String,
    /// Device address on the bus.
    pub device_address: u8,
    /// Hub port path from the root hub.
    pub port_chain: Vec<u8>,
    /// ADB interface number, when visible without opening the device
    /// (`None` e.g. for Windows devices bound entirely to WinUSB).
    pub interface_number: Option<u8>,
    #[cfg(not(target_os = "android"))]
    info: nusb::DeviceInfo,
}

impl UsbDeviceInfo {
    /// Physical location like adb's `usb:` path, e.g. `1-2.4`.
    pub fn location(&self) -> String {
        let ports: Vec<String> = self.port_chain.iter().map(u8::to_string).collect();
        if ports.is_empty() {
            format!("{}-{}", self.bus_id, self.device_address)
        } else {
            format!("{}-{}", self.bus_id, ports.join("."))
        }
    }

    /// The serial adb would use: the USB serial number, or `usb:<location>`.
    pub fn adb_serial(&self) -> String {
        match &self.serial {
            Some(s) if !s.is_empty() => s.clone(),
            _ => format!("usb:{}", self.location()),
        }
    }
}

/// List USB devices with an ADB interface, without opening them.
///
/// Not available on Android (enumerate with `UsbManager` and use [`UsbTransport::from_fd`]).
pub async fn list_adb_devices() -> Result<Vec<UsbDeviceInfo>> {
    #[cfg(not(target_os = "android"))]
    {
        let devices = nusb::list_devices()
            .await
            .map_err(|e| usb_error("listing USB devices", e))?;
        Ok(devices.filter_map(adb_candidate).collect())
    }
    #[cfg(target_os = "android")]
    {
        Err(AdbError::Io(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "USB enumeration is done by Android's UsbManager; pass the device fd to UsbTransport::from_fd",
        )))
    }
}

#[cfg(not(target_os = "android"))]
fn adb_candidate(info: nusb::DeviceInfo) -> Option<UsbDeviceInfo> {
    let adb_interface = info
        .interfaces()
        .find(|i| {
            i.class() == ADB_CLASS && i.subclass() == ADB_SUBCLASS && i.protocol() == ADB_PROTOCOL
        })
        .map(|i| i.interface_number());
    #[cfg(target_os = "windows")]
    let maybe_adb = adb_interface.is_some()
        || (info.interfaces().next().is_none()
            && info
                .driver()
                .is_some_and(|d| d.eq_ignore_ascii_case("winusb"))
            && info.class() == 0);
    #[cfg(not(target_os = "windows"))]
    let maybe_adb = adb_interface.is_some();
    if !maybe_adb {
        return None;
    }
    Some(UsbDeviceInfo {
        vendor_id: info.vendor_id(),
        product_id: info.product_id(),
        serial: info.serial_number().map(str::to_string),
        manufacturer: info.manufacturer_string().map(str::to_string),
        product: info.product_string().map(str::to_string),
        bus_id: info.bus_id().to_string(),
        device_address: info.device_address(),
        port_chain: info.port_chain().to_vec(),
        interface_number: adb_interface,
        info,
    })
}

/// An opened and claimed ADB interface.
pub struct UsbTransport {
    reader: UsbReader,
    writer: UsbWriter,
    serial: String,
}

impl std::fmt::Debug for UsbTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsbTransport")
            .field("serial", &self.serial)
            .finish_non_exhaustive()
    }
}

impl UsbTransport {
    /// Open a device found by [`list_adb_devices`] and claim its ADB interface.
    ///
    /// Fails with [`std::io::ErrorKind::ResourceBusy`] when another program
    /// (usually a running adb server) holds the interface; see [`is_busy`].
    #[cfg(not(target_os = "android"))]
    pub async fn open(info: &UsbDeviceInfo) -> Result<UsbTransport> {
        let device = info
            .info
            .open()
            .await
            .map_err(|e| usb_error("opening USB device", e))?;
        UsbTransport::from_device(device, Some(info.adb_serial())).await
    }

    /// Wrap a usbfs file descriptor that is already open, e.g. from Android's
    /// `UsbManager.openDevice(device).getFileDescriptor()`.
    ///
    /// The descriptor is owned (and closed) by the transport. From JNI, pass a
    /// duplicate so the Java `UsbDeviceConnection` keeps its own:
    /// `unsafe { BorrowedFd::borrow_raw(fd) }.try_clone_to_owned()`.
    /// `serial` should be `UsbDevice.getSerialNumber()`; if `None` it is read
    /// from the device descriptor.
    #[cfg(any(target_os = "android", target_os = "linux"))]
    pub async fn from_fd(fd: std::os::fd::OwnedFd, serial: Option<String>) -> Result<UsbTransport> {
        let device = nusb::Device::from_fd(fd)
            .await
            .map_err(|e| usb_error("opening USB fd", e))?;
        UsbTransport::from_device(device, serial).await
    }

    /// Claim the ADB interface of an opened `nusb` device.
    pub async fn from_device(device: nusb::Device, serial: Option<String>) -> Result<UsbTransport> {
        let found = match find_adb_interface(&device) {
            Some(found) => Some(found),
            None => {
                // macOS leaves vendor-class devices unconfigured; select configuration 1 and retry.
                #[cfg(not(target_os = "windows"))]
                if device.active_configuration().is_err() {
                    device
                        .set_configuration(1)
                        .await
                        .map_err(|e| usb_error("configuring USB device", e))?;
                }
                find_adb_interface(&device)
            }
        };
        let Some(found) = found else {
            return Err(AdbError::Other(
                "the USB device has no ADB interface (is USB debugging enabled?)".into(),
            ));
        };

        let interface = device
            .claim_interface(found.interface)
            .await
            .map_err(|e| usb_error("claiming the ADB interface", e))?;
        if found.alt_setting != 0 {
            interface
                .set_alt_setting(found.alt_setting)
                .await
                .map_err(|e| usb_error("selecting the ADB alternate setting", e))?;
        }
        let mut ep_in = interface
            .endpoint::<Bulk, In>(found.ep_in)
            .map_err(|e| usb_error("opening the bulk IN endpoint", e))?;
        let mut ep_out = interface
            .endpoint::<Bulk, Out>(found.ep_out)
            .map_err(|e| usb_error("opening the bulk OUT endpoint", e))?;
        // Reset data toggles / stalls left over from a previous host session (adb does the same).
        if let Err(e) = ep_in.clear_halt().await {
            tracing::debug!("clear_halt on ADB IN endpoint failed: {e}");
        }
        if let Err(e) = ep_out.clear_halt().await {
            tracing::debug!("clear_halt on ADB OUT endpoint failed: {e}");
        }

        let serial = match serial {
            Some(s) if !s.is_empty() => s,
            _ => read_serial(&device)
                .await
                .unwrap_or_else(|| "usb".to_string()),
        };
        Ok(UsbTransport {
            reader: UsbReader::new(NusbIn {
                ep: ep_in,
                _interface: interface.clone(),
            }),
            writer: UsbWriter::new(NusbOut {
                ep: ep_out,
                _interface: interface,
            }),
            serial,
        })
    }

    /// The device serial (USB iSerialNumber).
    pub fn serial(&self) -> &str {
        &self.serial
    }

    /// Split into message reader/writer halves.
    pub fn into_split(self) -> (UsbReader, UsbWriter) {
        (self.reader, self.writer)
    }
}

/// Whether an error means the USB interface is held by another program
/// (typically a running adb server), so the adb-server transport should be used.
pub fn is_busy(err: &AdbError) -> bool {
    match err {
        AdbError::Io(e) => {
            e.kind() == std::io::ErrorKind::ResourceBusy
                || (cfg!(target_os = "windows") && e.kind() == std::io::ErrorKind::PermissionDenied)
        }
        _ => false,
    }
}

struct FoundInterface {
    interface: u8,
    alt_setting: u8,
    ep_in: u8,
    ep_out: u8,
}

fn find_adb_interface(device: &nusb::Device) -> Option<FoundInterface> {
    let config = device.active_configuration().ok()?;
    config.interface_alt_settings().find_map(|alt| {
        if alt.class() != ADB_CLASS
            || alt.subclass() != ADB_SUBCLASS
            || alt.protocol() != ADB_PROTOCOL
        {
            return None;
        }
        let bulk = |dir: Direction| {
            alt.endpoints()
                .find(|e| {
                    e.transfer_type() == nusb::descriptors::TransferType::Bulk
                        && e.direction() == dir
                })
                .map(|e| e.address())
        };
        Some(FoundInterface {
            interface: alt.interface_number(),
            alt_setting: alt.alternate_setting(),
            ep_in: bulk(Direction::In)?,
            ep_out: bulk(Direction::Out)?,
        })
    })
}

async fn read_serial(device: &nusb::Device) -> Option<String> {
    let index = device.device_descriptor().serial_number_string_index()?;
    device
        .get_string_descriptor(
            index,
            nusb::descriptors::language_id::US_ENGLISH,
            std::time::Duration::from_secs(1),
        )
        .await
        .ok()
        .filter(|s| !s.is_empty())
}

fn usb_error(context: &str, e: nusb::Error) -> AdbError {
    use std::io::{Error, ErrorKind};
    match e.kind() {
        nusb::ErrorKind::Busy => AdbError::Io(Error::new(
            ErrorKind::ResourceBusy,
            format!(
                "{context}: the USB interface is busy, probably claimed by a running adb server; \
                 connect through the adb server instead ({e})"
            ),
        )),
        nusb::ErrorKind::PermissionDenied => AdbError::Io(Error::new(
            ErrorKind::PermissionDenied,
            if cfg!(target_os = "windows") {
                format!(
                    "{context}: access denied; the device may be in use by an adb server, or its ADB \
                     interface is not bound to the WinUSB driver ({e})"
                )
            } else {
                format!(
                    "{context}: permission denied; on Linux add a udev rule for the headset ({e})"
                )
            },
        )),
        nusb::ErrorKind::Disconnected => AdbError::Disconnected,
        nusb::ErrorKind::NotFound => {
            AdbError::Io(Error::new(ErrorKind::NotFound, format!("{context}: {e}")))
        }
        nusb::ErrorKind::Unsupported => AdbError::Io(Error::new(
            ErrorKind::Unsupported,
            format!("{context}: {e}"),
        )),
        _ => AdbError::Io(Error::other(format!("{context}: {e}"))),
    }
}

fn transfer_error(e: TransferError) -> AdbError {
    match e {
        TransferError::Disconnected => AdbError::Disconnected,
        TransferError::Cancelled => AdbError::Io(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "USB transfer cancelled",
        )),
        other => AdbError::Io(std::io::Error::other(format!(
            "USB transfer failed: {other}"
        ))),
    }
}

/// A bulk IN pipe: the part of an endpoint the ADB reader needs.
pub(crate) trait BulkIn: Send + 'static {
    /// Endpoint max packet size.
    fn max_packet_size(&self) -> usize;
    /// Request `len` bytes (a multiple of the max packet size) and return what
    /// arrived; the transfer ends early at a short or zero-length packet. If a
    /// transfer is still in flight (its future was dropped), its data is
    /// returned instead of submitting a new one.
    fn transfer(&mut self, len: usize)
    -> impl std::future::Future<Output = Result<Vec<u8>>> + Send;
}

/// A bulk OUT pipe.
pub(crate) trait BulkOut: Send + 'static {
    /// Endpoint max packet size.
    fn max_packet_size(&self) -> usize;
    /// Queue a transfer (an empty one is a zero-length packet).
    fn submit(&mut self, data: Vec<u8>);
    /// Wait for all queued transfers; the first error wins.
    fn finish(&mut self) -> impl std::future::Future<Output = Result<()>> + Send;
}

/// `nusb` bulk IN endpoint.
pub struct NusbIn {
    ep: Endpoint<Bulk, In>,
    _interface: Interface,
}

impl BulkIn for NusbIn {
    fn max_packet_size(&self) -> usize {
        self.ep.max_packet_size().max(1)
    }

    async fn transfer(&mut self, len: usize) -> Result<Vec<u8>> {
        if self.ep.pending() == 0 {
            self.ep.submit(Buffer::new(len));
        }
        let completion = self.ep.next_complete().await;
        completion.status.map_err(transfer_error)?;
        Ok(completion.buffer.into_vec())
    }
}

/// `nusb` bulk OUT endpoint.
pub struct NusbOut {
    ep: Endpoint<Bulk, Out>,
    _interface: Interface,
}

impl BulkOut for NusbOut {
    fn max_packet_size(&self) -> usize {
        self.ep.max_packet_size().max(1)
    }

    fn submit(&mut self, data: Vec<u8>) {
        self.ep.submit(Buffer::from(data));
    }

    async fn finish(&mut self) -> Result<()> {
        let mut result = Ok(());
        while self.ep.pending() > 0 {
            let completion = self.ep.next_complete().await;
            if let Err(e) = completion.status
                && result.is_ok()
            {
                result = Err(transfer_error(e));
            }
        }
        result
    }
}

/// Reading half of a [`UsbTransport`].
#[allow(private_bounds)]
pub struct UsbReader<P: BulkIn = NusbIn> {
    pipe: P,
    leftover: Bytes,
}

#[allow(private_bounds)]
impl<P: BulkIn> UsbReader<P> {
    pub(crate) fn new(pipe: P) -> Self {
        UsbReader {
            pipe,
            leftover: Bytes::new(),
        }
    }

    /// Read exactly `n` bytes, treating the IN pipe as a byte stream but never
    /// requesting more than needed (rounded up to the packet size), so a device
    /// that ends a transfer on a packet boundary without a zero-length packet
    /// cannot leave the read waiting for data that is not coming.
    async fn read_exact(&mut self, n: usize) -> Result<Bytes> {
        if self.leftover.len() >= n {
            return Ok(self.leftover.split_to(n));
        }
        let mps = self.pipe.max_packet_size();
        let cap = (MAX_IN_TRANSFER / mps).max(1) * mps;
        let mut out = BytesMut::with_capacity(n);
        out.extend_from_slice(&self.leftover);
        self.leftover = Bytes::new();
        while out.len() < n {
            let need = n - out.len();
            let request = (need.div_ceil(mps) * mps).min(cap);
            let data = self.pipe.transfer(request).await?;
            if data.is_empty() {
                continue; // zero-length packet terminating a previous transfer
            }
            if out.is_empty() && data.len() == need {
                return Ok(Bytes::from(data));
            }
            if data.len() <= need {
                out.extend_from_slice(&data);
            } else {
                out.extend_from_slice(&data[..need]);
                self.leftover = Bytes::from(data).slice(need..);
            }
        }
        Ok(out.freeze())
    }
}

#[allow(private_bounds)]
impl<P: BulkIn> MessageReader for UsbReader<P> {
    async fn read_message(&mut self, verify_checksum: bool) -> Result<Message> {
        let raw = self.read_exact(HEADER_LEN).await?;
        let mut header_bytes = [0u8; HEADER_LEN];
        header_bytes.copy_from_slice(&raw);
        let header = Header::decode(&header_bytes)?;
        check_length(&header)?;
        let payload = if header.data_length == 0 {
            Bytes::new()
        } else {
            self.read_exact(header.data_length as usize).await?
        };
        Message::from_parts(header, payload, verify_checksum)
    }
}

/// Writing half of a [`UsbTransport`].
#[allow(private_bounds)]
pub struct UsbWriter<P: BulkOut = NusbOut> {
    pipe: P,
}

#[allow(private_bounds)]
impl<P: BulkOut> UsbWriter<P> {
    pub(crate) fn new(pipe: P) -> Self {
        UsbWriter { pipe }
    }

    /// Queue one transfer, followed by a zero-length packet if it ends on a
    /// packet boundary (adb's `zero_mask` rule), so the device sees where it ends.
    fn submit(&mut self, data: Vec<u8>) {
        let zlp = data.len().is_multiple_of(self.pipe.max_packet_size());
        self.pipe.submit(data);
        if zlp {
            self.pipe.submit(Vec::new());
        }
    }
}

#[allow(private_bounds)]
impl<P: BulkOut> MessageWriter for UsbWriter<P> {
    async fn write_message(&mut self, msg: &Message, with_checksum: bool) -> Result<()> {
        self.submit(msg.header(with_checksum).encode().to_vec());
        if !msg.payload.is_empty() {
            self.submit(msg.payload.to_vec());
        }
        self.pipe.finish().await
    }

    async fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{A_OKAY, A_WRTE};
    use std::collections::VecDeque;

    /// Simulates a device's bulk IN pipe at packet level: each device-side
    /// write is split into packets; a host request completes when it is full
    /// or a short/zero-length packet arrives.
    struct SimIn {
        mps: usize,
        packets: VecDeque<Vec<u8>>,
        requests: Vec<usize>,
    }

    impl SimIn {
        fn new(mps: usize, writes: &[Vec<u8>], device_zlp: bool) -> Self {
            let mut packets = VecDeque::new();
            for w in writes {
                for chunk in w.chunks(mps) {
                    packets.push_back(chunk.to_vec());
                }
                if w.is_empty() || (device_zlp && w.len().is_multiple_of(mps)) {
                    packets.push_back(Vec::new());
                }
            }
            SimIn {
                mps,
                packets,
                requests: Vec::new(),
            }
        }
    }

    impl BulkIn for SimIn {
        fn max_packet_size(&self) -> usize {
            self.mps
        }

        async fn transfer(&mut self, len: usize) -> Result<Vec<u8>> {
            assert!(
                len > 0 && len.is_multiple_of(self.mps),
                "IN request {len} not a multiple of {}",
                self.mps
            );
            self.requests.push(len);
            let mut out = Vec::new();
            loop {
                let Some(packet) = self.packets.pop_front() else {
                    // On real hardware this transfer would wait forever.
                    assert!(
                        !out.is_empty(),
                        "read would hang: request of {len} bytes with no data left"
                    );
                    panic!(
                        "read would hang: request of {len} bytes got only {} bytes and no short packet",
                        out.len()
                    );
                };
                let short = packet.len() < self.mps;
                out.extend_from_slice(&packet);
                if short || out.len() == len {
                    return Ok(out);
                }
            }
        }
    }

    #[derive(Default)]
    struct SimOut {
        mps: usize,
        transfers: Vec<Vec<u8>>,
    }

    impl BulkOut for SimOut {
        fn max_packet_size(&self) -> usize {
            self.mps
        }

        fn submit(&mut self, data: Vec<u8>) {
            self.transfers.push(data);
        }

        async fn finish(&mut self) -> Result<()> {
            Ok(())
        }
    }

    fn device_writes(messages: &[Message], merge: bool) -> Vec<Vec<u8>> {
        let mut writes = Vec::new();
        for m in messages {
            let header = m.header(false).encode().to_vec();
            if merge {
                writes.push(m.encode(false));
            } else {
                writes.push(header);
                if !m.payload.is_empty() {
                    // adbd writes payloads in 16 KiB pieces.
                    for piece in m.payload.chunks(16 * 1024) {
                        writes.push(piece.to_vec());
                    }
                }
            }
        }
        writes
    }

    fn sample_messages(mps: usize) -> Vec<Message> {
        vec![
            Message::new(A_OKAY, 1, 2, Bytes::new()),
            Message::new(A_WRTE, 1, 2, vec![1u8; mps]), // exactly one packet
            Message::new(A_WRTE, 1, 2, vec![2u8; 3 * mps]), // packet multiple
            Message::new(A_WRTE, 1, 2, vec![3u8; 100]), // short
            Message::new(A_WRTE, 1, 2, vec![4u8; 64 * 1024]), // several 16 KiB device writes
            Message::new(A_WRTE, 1, 2, vec![5u8; 1024 * 1024]), // max payload
            Message::new(A_WRTE, 1, 2, vec![6u8; 16 * 1024 + 1]),
            Message::new(A_OKAY, 3, 4, Bytes::new()),
        ]
    }

    async fn read_all(sim: SimIn, count: usize) -> (Vec<Message>, Vec<usize>) {
        let mut reader = UsbReader::new(sim);
        let mut got = Vec::new();
        for _ in 0..count {
            got.push(reader.read_message(false).await.unwrap());
        }
        assert!(
            reader.pipe.packets.iter().all(|p| p.is_empty()),
            "unread data left"
        );
        (got, reader.pipe.requests)
    }

    #[tokio::test]
    async fn reads_without_device_zlps() {
        for mps in [64, 512, 1024] {
            let msgs = sample_messages(mps);
            let sim = SimIn::new(mps, &device_writes(&msgs, false), false);
            let (got, requests) = read_all(sim, msgs.len()).await;
            assert_eq!(got, msgs);
            assert!(requests.iter().all(|r| *r <= MAX_IN_TRANSFER));

            // The last thing on the wire is a packet-multiple payload without a
            // ZLP (the device now waits for our OKAY): reading must not need more.
            let last = vec![Message::new(A_WRTE, 1, 2, vec![9u8; 2 * mps])];
            let sim = SimIn::new(mps, &device_writes(&last, false), false);
            let (got, requests) = read_all(sim, 1).await;
            assert_eq!(got, last);
            assert_eq!(requests, vec![mps, 2 * mps]);
        }
    }

    #[tokio::test]
    async fn reads_with_device_zlps() {
        for mps in [64, 512, 1024] {
            let msgs = sample_messages(mps);
            let sim = SimIn::new(mps, &device_writes(&msgs, false), true);
            let (got, _) = read_all(sim, msgs.len()).await;
            assert_eq!(got, msgs);
        }
    }

    #[tokio::test]
    async fn reads_header_and_payload_merged_in_one_transfer() {
        let mps = 512;
        // Avoid messages whose merged size is a packet multiple without ZLP
        // followed by nothing (that would be ambiguous on the wire too).
        let msgs = vec![
            Message::new(A_WRTE, 1, 2, vec![7u8; 1000]),
            Message::new(A_OKAY, 1, 2, Bytes::new()),
            Message::new(A_WRTE, 1, 2, vec![8u8; 70_000]),
            Message::new(A_OKAY, 1, 2, Bytes::new()),
        ];
        let sim = SimIn::new(mps, &device_writes(&msgs, true), true);
        let (got, _) = read_all(sim, msgs.len()).await;
        assert_eq!(got, msgs);
    }

    #[tokio::test]
    async fn writes_add_zero_length_packets_on_packet_boundaries() {
        let mut writer = UsbWriter::new(SimOut {
            mps: 512,
            ..SimOut::default()
        });
        writer
            .write_message(&Message::new(A_WRTE, 1, 2, vec![0u8; 1024]), false)
            .await
            .unwrap();
        writer
            .write_message(&Message::new(A_WRTE, 1, 2, vec![0u8; 100]), false)
            .await
            .unwrap();
        writer
            .write_message(&Message::new(A_OKAY, 1, 2, Bytes::new()), false)
            .await
            .unwrap();
        let sizes: Vec<usize> = writer.pipe.transfers.iter().map(Vec::len).collect();
        assert_eq!(sizes, vec![24, 1024, 0, 24, 100, 24]);

        // With an 8-byte packet size even the 24-byte header needs a ZLP.
        let mut writer = UsbWriter::new(SimOut {
            mps: 8,
            ..SimOut::default()
        });
        writer
            .write_message(&Message::new(A_OKAY, 1, 2, Bytes::new()), false)
            .await
            .unwrap();
        let sizes: Vec<usize> = writer.pipe.transfers.iter().map(Vec::len).collect();
        assert_eq!(sizes, vec![24, 0]);
    }
}
