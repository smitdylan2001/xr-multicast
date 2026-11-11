//! Incremental parser for the scrcpy v5 video socket.
//!
//! After the dummy byte (handled by the session) the stream is:
//!
//! * device meta: 64 bytes, NUL-padded UTF-8 device name;
//! * codec id: `u32` BE (`0` = stream disabled, `1` = encoder configuration error);
//! * then repeated 12-byte headers:
//!   * MSB set: session packet: `u32` flags (bit 0: client resized), `u32` width, `u32` height;
//!   * MSB clear: media packet: `u64` `[0|config|key|pts:61]`, `u32` size, then `size` bytes.
//!
//! The parser is chunk-size independent: feed it whatever the socket returns.

use bytes::{Buf, Bytes, BytesMut};

/// Length of the device name field.
pub const DEVICE_NAME_LENGTH: usize = 64;
/// Default upper bound for one media packet (a 4K key frame is a few MB).
pub const DEFAULT_MAX_PACKET_SIZE: usize = 64 << 20;

const SESSION_FLAG: u32 = 0x8000_0000;
const CLIENT_RESIZED_FLAG: u32 = 1;
const CONFIG_FLAG: u64 = 1 << 62;
const KEY_FLAG: u64 = 1 << 61;
const PTS_MASK: u64 = KEY_FLAG - 1;

/// Fatal stream errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DemuxError {
    /// Codec id 1: the encoder rejected its configuration.
    #[error("The headset could not configure its video encoder.")]
    EncoderFailed,
    /// Codec id 0: the video stream is disabled.
    #[error("The headset disabled the video stream.")]
    StreamDisabled,
    /// A media packet header announced an implausibly large payload.
    #[error("media packet of {size} bytes exceeds the {max} byte limit")]
    PacketTooLarge {
        /// Announced size.
        size: u32,
        /// Configured limit.
        max: usize,
    },
}

/// One encoded media packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaPacket {
    /// Codec configuration (SPS/PPS/VPS, `av1C`...), not a frame.
    pub config: bool,
    /// Key frame (never set together with `config`).
    pub key: bool,
    /// Presentation timestamp in microseconds (0 for config packets).
    pub pts: u64,
    /// The payload.
    pub data: Bytes,
}

/// What the parser found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DemuxEvent {
    /// The device name from the device meta.
    DeviceName(String),
    /// The stream codec id (see [`crate::VideoCodec::from_id`] / [`crate::codec_id_name`]).
    Codec(u32),
    /// A new capture session: the frame size changed (e.g. rotation).
    Session {
        /// Encoded width in pixels.
        width: u32,
        /// Encoded height in pixels.
        height: u32,
        /// The size changed because the client asked for it (flex displays).
        client_resized: bool,
    },
    /// A media packet.
    Packet(MediaPacket),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    DeviceMeta,
    Codec,
    Header,
    Payload {
        config: bool,
        key: bool,
        pts: u64,
        size: usize,
    },
    Dead,
}

/// Incremental scrcpy video stream parser.
///
/// ```
/// use xrmc_scrcpy::{Demuxer, DemuxEvent};
/// let mut d = Demuxer::new();
/// d.push(&[0u8; 64]);
/// d.push(&0x6832_3634u32.to_be_bytes());
/// assert_eq!(d.next_event(), Ok(Some(DemuxEvent::DeviceName(String::new()))));
/// assert_eq!(d.next_event(), Ok(Some(DemuxEvent::Codec(0x6832_3634))));
/// assert_eq!(d.next_event(), Ok(None)); // needs more data
/// ```
#[derive(Debug)]
pub struct Demuxer {
    buf: BytesMut,
    state: State,
    max_packet_size: usize,
}

impl Default for Demuxer {
    fn default() -> Self {
        Self::new()
    }
}

impl Demuxer {
    /// A parser expecting the device meta, with [`DEFAULT_MAX_PACKET_SIZE`].
    pub fn new() -> Self {
        Self::with_max_packet_size(DEFAULT_MAX_PACKET_SIZE)
    }

    /// A parser that rejects media packets larger than `max` bytes, which
    /// bounds its memory use on a corrupt stream.
    pub fn with_max_packet_size(max: usize) -> Self {
        Self {
            buf: BytesMut::new(),
            state: State::DeviceMeta,
            max_packet_size: max,
        }
    }

    /// Append received bytes.
    pub fn push(&mut self, chunk: &[u8]) {
        if self.state != State::Dead {
            self.buf.extend_from_slice(chunk);
        }
    }

    /// The receive buffer, to read socket data into directly (zero-copy
    /// payloads). Bytes appended here are parsed by [`Demuxer::next_event`].
    pub fn buffer_mut(&mut self) -> &mut BytesMut {
        &mut self.buf
    }

    /// Bytes still missing before the next event can be produced (a read
    /// size hint; 0 if an event is ready or the stream is dead).
    pub fn bytes_needed(&self) -> usize {
        let want = match self.state {
            State::DeviceMeta => DEVICE_NAME_LENGTH,
            State::Codec => 4,
            State::Header => 12,
            State::Payload { size, .. } => size,
            State::Dead => 0,
        };
        want.saturating_sub(self.buf.len())
    }

    /// Whether the stream hit a fatal error (it then produces no more events).
    pub fn is_dead(&self) -> bool {
        self.state == State::Dead
    }

    /// The next complete event, `Ok(None)` if more data is needed. An error
    /// is reported once; afterwards the parser stays dead and returns `Ok(None)`.
    pub fn next_event(&mut self) -> Result<Option<DemuxEvent>, DemuxError> {
        loop {
            match self.state {
                State::DeviceMeta => {
                    if self.buf.len() < DEVICE_NAME_LENGTH {
                        return Ok(None);
                    }
                    let raw = self.buf.split_to(DEVICE_NAME_LENGTH);
                    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
                    self.state = State::Codec;
                    return Ok(Some(DemuxEvent::DeviceName(
                        String::from_utf8_lossy(&raw[..end]).into_owned(),
                    )));
                }
                State::Codec => {
                    if self.buf.len() < 4 {
                        return Ok(None);
                    }
                    let id = self.buf.get_u32();
                    return match id {
                        0 => Err(self.die(DemuxError::StreamDisabled)),
                        1 => Err(self.die(DemuxError::EncoderFailed)),
                        _ => {
                            self.state = State::Header;
                            Ok(Some(DemuxEvent::Codec(id)))
                        }
                    };
                }
                State::Header => {
                    if self.buf.len() < 12 {
                        return Ok(None);
                    }
                    let hi =
                        u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]);
                    if hi & SESSION_FLAG != 0 {
                        self.buf.advance(4);
                        let width = self.buf.get_u32();
                        let height = self.buf.get_u32();
                        return Ok(Some(DemuxEvent::Session {
                            width,
                            height,
                            client_resized: hi & CLIENT_RESIZED_FLAG != 0,
                        }));
                    }
                    let pts_flags = self.buf.get_u64();
                    let size = self.buf.get_u32();
                    if size as usize > self.max_packet_size {
                        return Err(self.die(DemuxError::PacketTooLarge {
                            size,
                            max: self.max_packet_size,
                        }));
                    }
                    let config = pts_flags & CONFIG_FLAG != 0;
                    self.state = State::Payload {
                        config,
                        key: pts_flags & KEY_FLAG != 0 && !config,
                        pts: pts_flags & PTS_MASK,
                        size: size as usize,
                    };
                    self.buf
                        .reserve((size as usize).saturating_sub(self.buf.len()));
                }
                State::Payload {
                    config,
                    key,
                    pts,
                    size,
                } => {
                    if self.buf.len() < size {
                        return Ok(None);
                    }
                    let data = self.buf.split_to(size).freeze();
                    self.state = State::Header;
                    return Ok(Some(DemuxEvent::Packet(MediaPacket {
                        config,
                        key,
                        pts,
                        data,
                    })));
                }
                State::Dead => return Ok(None),
            }
        }
    }

    fn die(&mut self, e: DemuxError) -> DemuxError {
        self.state = State::Dead;
        self.buf = BytesMut::new();
        e
    }
}

#[cfg(test)]
pub(crate) mod test_util {
    /// 64-byte NUL-padded device meta.
    pub fn device_meta(name: &str) -> Vec<u8> {
        let mut b = vec![0u8; 64];
        b[..name.len()].copy_from_slice(name.as_bytes());
        b
    }

    /// A session packet header.
    pub fn session(w: u32, h: u32) -> Vec<u8> {
        [0x8000_0000u32, w, h]
            .iter()
            .flat_map(|v| v.to_be_bytes())
            .collect()
    }

    /// A media packet (header + payload).
    pub fn media(data: &[u8], config: bool, key: bool, pts: u64) -> Vec<u8> {
        let mut v = if config { 1u64 << 62 } else { pts };
        if key {
            v |= 1 << 61;
        }
        let mut out = v.to_be_bytes().to_vec();
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(data);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::*;
    use super::*;

    #[derive(Default, Debug)]
    struct Collected {
        device: Option<String>,
        codec: Option<u32>,
        sessions: Vec<(u32, u32, bool)>,
        packets: Vec<MediaPacket>,
        errors: Vec<DemuxError>,
    }

    fn collect(stream: &[u8], chunk: usize) -> Collected {
        let mut d = Demuxer::new();
        let mut ev = Collected::default();
        for c in stream.chunks(chunk) {
            d.push(c);
            loop {
                match d.next_event() {
                    Ok(None) => break,
                    Ok(Some(DemuxEvent::DeviceName(n))) => ev.device = Some(n),
                    Ok(Some(DemuxEvent::Codec(id))) => ev.codec = Some(id),
                    Ok(Some(DemuxEvent::Session {
                        width,
                        height,
                        client_resized,
                    })) => ev.sessions.push((width, height, client_resized)),
                    Ok(Some(DemuxEvent::Packet(p))) => ev.packets.push(p),
                    Err(e) => ev.errors.push(e),
                }
            }
        }
        ev
    }

    const SPS: [u8; 15] = [
        0, 0, 0, 1, 0x67, 0x64, 0x00, 0x33, 0xaa, 0, 0, 0, 1, 0x68, 0xce,
    ];

    fn stream() -> Vec<u8> {
        [
            device_meta("Quest 3"),
            0x6832_3634u32.to_be_bytes().to_vec(),
            session(3664, 1920),
            media(&SPS, true, false, 0),
            media(&[7; 5000], false, true, 16666),
            media(&[3; 300], false, false, 33333),
            session(1920, 1006),
            media(&[3; 300], false, false, 50000),
        ]
        .concat()
    }

    // Port of 'parses scrcpy v5 video stream (chunk size N)' (test/stream-parser.test.js).
    #[test]
    fn parses_scrcpy_v5_video_stream_at_any_chunk_size() {
        let stream = stream();
        for chunk in [1, 7, 64, 4096, stream.len()] {
            let ev = collect(&stream, chunk);
            assert!(ev.errors.is_empty(), "chunk {chunk}");
            assert_eq!(ev.device.as_deref(), Some("Quest 3"));
            assert_eq!(ev.codec.map(crate::codec_id_name).as_deref(), Some("h264"));
            assert_eq!(ev.sessions, [(3664, 1920, false), (1920, 1006, false)]);
            assert_eq!(
                ev.packets
                    .iter()
                    .map(|p| (p.config, p.key, p.pts, p.data.len()))
                    .collect::<Vec<_>>(),
                [
                    (true, false, 0, SPS.len()),
                    (false, true, 16666, 5000),
                    (false, false, 33333, 300),
                    (false, false, 50000, 300)
                ]
            );
            assert!(ev.packets[1].data.iter().all(|&b| b == 7));
            assert_eq!(&ev.packets[0].data[..], SPS);
        }
    }

    // Port of 'reports encoder failure codec id'.
    #[test]
    fn reports_encoder_failure_codec_id() {
        let ev = collect(&[device_meta("x"), 1u32.to_be_bytes().to_vec()].concat(), 3);
        assert_eq!(ev.errors, [DemuxError::EncoderFailed]);
        let ev = collect(
            &[device_meta("x"), 0u32.to_be_bytes().to_vec(), session(1, 1)].concat(),
            5,
        );
        assert_eq!(ev.errors, [DemuxError::StreamDisabled]);
        assert!(ev.sessions.is_empty());
    }

    // Replaces 'ByteQueue takes across chunk boundaries': payloads split over pushes.
    #[test]
    fn payload_across_chunk_boundaries() {
        let mut d = Demuxer::new();
        d.push(&device_meta("a"));
        d.push(&0x0061_7631u32.to_be_bytes());
        assert!(matches!(
            d.next_event(),
            Ok(Some(DemuxEvent::DeviceName(_)))
        ));
        assert_eq!(d.next_event(), Ok(Some(DemuxEvent::Codec(0x0061_7631))));
        let pkt = media(&[1, 2, 3, 4, 5], false, true, 42);
        d.push(&pkt[..14]);
        assert_eq!(d.next_event(), Ok(None));
        assert_eq!(d.bytes_needed(), 3);
        d.push(&pkt[14..]);
        let Ok(Some(DemuxEvent::Packet(p))) = d.next_event() else {
            panic!()
        };
        assert_eq!(
            (p.key, p.pts, &p.data[..]),
            (true, 42, &[1, 2, 3, 4, 5][..])
        );
        assert_eq!(d.bytes_needed(), 12);
    }

    #[test]
    fn flags_and_pts_bits() {
        let mut d = Demuxer::new();
        d.push(&device_meta(""));
        d.push(&0x6832_3635u32.to_be_bytes());
        // client-resized session packet
        d.push(&[0x80, 0, 0, 1, 0, 0, 7, 0x80, 0, 0, 4, 0x38]);
        // config + key flags: key is cleared on config packets
        d.push(&media(&[9], true, true, 0));
        // largest pts
        let mut h = ((1u64 << 61) - 1).to_be_bytes().to_vec();
        h.extend_from_slice(&0u32.to_be_bytes());
        d.push(&h);
        let mut events = Vec::new();
        while let Some(e) = d.next_event().unwrap() {
            events.push(e);
        }
        assert_eq!(events[0], DemuxEvent::DeviceName(String::new()));
        assert_eq!(
            events[2],
            DemuxEvent::Session {
                width: 1920,
                height: 1080,
                client_resized: true
            }
        );
        let DemuxEvent::Packet(p) = &events[3] else {
            panic!()
        };
        assert!(p.config && !p.key);
        let DemuxEvent::Packet(p) = &events[4] else {
            panic!()
        };
        assert_eq!(
            (p.config, p.key, p.pts, p.data.len()),
            (false, false, (1 << 61) - 1, 0)
        );
    }

    #[test]
    fn device_name_utf8() {
        let ev = collect(
            &[
                device_meta("Pico 4 Ultra — ☃"),
                0x6832_3634u32.to_be_bytes().to_vec(),
            ]
            .concat(),
            9,
        );
        assert_eq!(ev.device.as_deref(), Some("Pico 4 Ultra — ☃"));
        // No NUL at all: the whole 64 bytes.
        let ev = collect(
            &[vec![b'a'; 64], 0x6832_3634u32.to_be_bytes().to_vec()].concat(),
            64,
        );
        assert_eq!(ev.device.unwrap().len(), 64);
    }

    #[test]
    fn oversized_packets_are_rejected() {
        let mut d = Demuxer::with_max_packet_size(1000);
        d.push(&device_meta("x"));
        d.push(&0x6832_3634u32.to_be_bytes());
        d.push(&media(&[0; 1001], false, false, 1));
        d.next_event().unwrap();
        d.next_event().unwrap();
        assert_eq!(
            d.next_event(),
            Err(DemuxError::PacketTooLarge {
                size: 1001,
                max: 1000
            })
        );
        assert!(d.is_dead());
        d.push(&[1, 2, 3]);
        assert_eq!(d.next_event(), Ok(None));
        assert!(d.buffer_mut().is_empty());
    }

    #[test]
    fn random_input_never_panics_and_stays_bounded() {
        let mut seed = 0x1234_5678_9abc_def0_u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for round in 0..300 {
            let mut d = Demuxer::with_max_packet_size(1 << 16);
            // Valid prefix half of the time, then random garbage in random chunks.
            if round % 2 == 0 {
                d.push(&device_meta("fuzz"));
                d.push(&0x6832_3634u32.to_be_bytes());
            }
            for _ in 0..50 {
                let len = (next() % 300) as usize;
                let chunk: Vec<u8> = (0..len).map(|_| next() as u8).collect();
                d.push(&chunk);
                while let Ok(Some(_)) = d.next_event() {}
                assert!(d.buffer_mut().len() <= (1 << 16) + 12 + 300 + 64);
            }
        }
    }
}
