//! An in-process fake adbd speaking the ADB wire protocol over a tokio duplex pipe.
//!
//! It implements the device side closely enough to exercise the client:
//! handshake with optional RSA auth (signature check, public-key prompt) or
//! STLS, old/new protocol versions (checksums), max-payload negotiation,
//! per-stream classic flow control (and checks that the client obeys it),
//! and a few services:
//!
//! * `echo:` / `localabstract:*` — echo data back
//! * `sink:` — swallow data, counting bytes
//! * `blackhole:` — swallow data but never acknowledge it
//! * `flood:<n>` — send `n` pattern bytes as fast as flow control allows
//! * `shell,v2,raw:<cmd>` / `shell:<cmd>` — a few fake commands
//! * `sync:` — SEND/DATA/DONE, STAT, QUIT (files kept in memory)
//! * `hang:` — never answer the OPEN
//! * `disconnect:` — drop the transport (cable pulled)
//! * `garbage:` — send a corrupt header
//! * anything else — refused with CLSE
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use rsa::PublicKey;
use tokio::io::{AsyncWriteExt, DuplexStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use xrmc_adb::protocol::*;
use xrmc_adb::shell::{ID_CLOSE_STDIN, ID_EXIT, ID_STDERR, ID_STDIN, ID_STDOUT, encode_v2_packet};
use xrmc_adb::transport::read_message_from;

pub const DEFAULT_BANNER: &str = "device::ro.product.name=fake;ro.product.model=Fake Quest;ro.product.device=fakedev;features=shell_v2,cmd,stat_v2";

/// How the fake device authenticates hosts.
#[derive(Clone)]
pub enum AuthMode {
    /// `ro.adb.secure=0`: answer CNXN right away.
    Disabled,
    /// Require a signature from one of `authorized`; if none matches, wait for
    /// the public key and accept it if `accept_new_key` (the user taps "Allow").
    Keys {
        authorized: Vec<PublicKey>,
        accept_new_key: bool,
    },
    /// Answer with STLS (Android 11+ wireless debugging).
    Tls,
}

/// When the device acknowledges the client's WRTEs.
#[derive(Clone, Copy, Debug)]
pub enum AckPolicy {
    Immediate,
    Delayed(Duration),
    Never,
}

#[derive(Clone)]
pub struct FakeConfig {
    pub version: u32,
    pub max_payload: u32,
    pub auth: AuthMode,
    pub banner: String,
    pub ack: AckPolicy,
    /// Size of the in-memory pipe buffer.
    pub pipe_size: usize,
    /// How long the simulated user takes to accept a new key.
    pub accept_delay: Duration,
}

impl Default for FakeConfig {
    fn default() -> Self {
        FakeConfig {
            version: A_VERSION,
            max_payload: MAX_PAYLOAD,
            auth: AuthMode::Disabled,
            banner: DEFAULT_BANNER.to_string(),
            ack: AckPolicy::Immediate,
            pipe_size: 256 * 1024,
            accept_delay: Duration::ZERO,
        }
    }
}

#[derive(Default)]
pub struct Stats {
    pub client_banner: Mutex<String>,
    pub client_max_payload: AtomicUsize,
    pub flow_violations: AtomicUsize,
    pub oversize_writes: AtomicUsize,
    pub checksum_errors: AtomicUsize,
    pub max_write_seen: AtomicUsize,
    pub signatures_checked: AtomicUsize,
    pub pubkey: Mutex<Option<Vec<u8>>>,
    pub opened: Mutex<Vec<String>>,
    pub closed_by_client: Mutex<Vec<String>>,
    pub refused_open_reply: AtomicUsize,
    pub sink_bytes: AtomicUsize,
    pub flood_sent: AtomicUsize,
    pub commands_ended_by_close: Mutex<Vec<String>>,
    pub transport_eof: AtomicBool,
    pub reverse_refused: AtomicBool,
}

impl Stats {
    pub fn closed(&self) -> Vec<String> {
        self.closed_by_client.lock().unwrap().clone()
    }
}

#[derive(Clone, Debug)]
pub struct FakeFile {
    pub mode: u32,
    pub data: Vec<u8>,
    pub mtime: u32,
}

pub struct Fake {
    pub stats: Arc<Stats>,
    pub files: Arc<Mutex<HashMap<String, FakeFile>>>,
    tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl Fake {
    /// Simulate unplugging: abort everything, dropping the pipe.
    pub fn unplug(&self) {
        for t in self.tasks.lock().unwrap().drain(..) {
            t.abort();
        }
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.unplug();
    }
}

enum Out {
    Msg(Message),
    Raw(Vec<u8>),
}

enum Ctl {
    Incoming(Message),
    Ack(u32),
    Remove(u32),
    Eof,
}

struct DevStream {
    client_id: u32,
    service: String,
    inbound: Option<mpsc::UnboundedSender<Bytes>>,
    okays: Option<mpsc::UnboundedSender<()>>,
    unacked: bool,
    ack: AckPolicy,
}

/// Serve fake devices on a TCP listener (one per accepted connection).
pub async fn serve_tcp(config: FakeConfig) -> (std::net::SocketAddr, Arc<Mutex<Vec<Fake>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let fakes = Arc::new(Mutex::new(Vec::new()));
    let fakes_task = fakes.clone();
    tokio::spawn(async move {
        while let Ok((mut tcp, _)) = listener.accept().await {
            let (mut pipe, fake) = start(config.clone());
            fakes_task.lock().unwrap().push(fake);
            tokio::spawn(async move {
                let _ = tokio::io::copy_bidirectional(&mut tcp, &mut pipe).await;
            });
        }
    });
    (addr, fakes)
}

/// Start a fake device; returns the host end of the pipe.
pub fn start(config: FakeConfig) -> (DuplexStream, Fake) {
    let (host_end, device_end) = tokio::io::duplex(config.pipe_size);
    let stats = Arc::new(Stats::default());
    let files = Arc::new(Mutex::new(HashMap::new()));
    let tasks = Arc::new(Mutex::new(Vec::new()));
    let fake = Fake {
        stats: stats.clone(),
        files: files.clone(),
        tasks: tasks.clone(),
    };
    let main = tokio::spawn(run(config, device_end, stats, files, tasks.clone()));
    tasks.lock().unwrap().push(main);
    (host_end, fake)
}

async fn run(
    config: FakeConfig,
    device_end: DuplexStream,
    stats: Arc<Stats>,
    files: Arc<Mutex<HashMap<String, FakeFile>>>,
    tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
) {
    let (mut rd, mut wr) = tokio::io::split(device_end);
    let old = config.version < A_VERSION_SKIP_CHECKSUM;

    // --- handshake (sequential) ---
    let Ok(cnxn) = read_message_from(&mut rd, true).await else {
        stats.checksum_errors.fetch_add(1, Ordering::SeqCst);
        return;
    };
    assert_eq!(cnxn.command, A_CNXN, "first message must be CNXN");
    *stats.client_banner.lock().unwrap() = String::from_utf8_lossy(&cnxn.payload).into_owned();
    stats
        .client_max_payload
        .store(cnxn.arg1 as usize, Ordering::SeqCst);
    let max_payload = config.max_payload.min(cnxn.arg1) as usize;
    let send = async |wr: &mut tokio::io::WriteHalf<DuplexStream>, msg: Message| {
        wr.write_all(&msg.encode(old)).await
    };
    let connect_msg = Message::new(
        A_CNXN,
        config.version,
        config.max_payload,
        config.banner.clone().into_bytes(),
    );
    match &config.auth {
        AuthMode::Disabled => {
            if send(&mut wr, connect_msg).await.is_err() {
                return;
            }
        }
        AuthMode::Tls => {
            let _ = send(
                &mut wr,
                Message::new(A_STLS, A_STLS_VERSION, 0, Bytes::new()),
            )
            .await;
            // Wait for the client to go away.
            while read_message_from(&mut rd, false).await.is_ok() {}
            return;
        }
        AuthMode::Keys {
            authorized,
            accept_new_key,
        } => {
            let mut token = [0u8; TOKEN_SIZE];
            token
                .iter_mut()
                .enumerate()
                .for_each(|(i, b)| *b = (i as u8).wrapping_mul(37).wrapping_add(11));
            if send(&mut wr, Message::new(A_AUTH, AUTH_TOKEN, 0, token.to_vec()))
                .await
                .is_err()
            {
                return;
            }
            loop {
                let Ok(msg) = read_message_from(&mut rd, old).await else {
                    return;
                };
                assert_eq!(msg.command, A_AUTH);
                match msg.arg0 {
                    AUTH_SIGNATURE => {
                        stats.signatures_checked.fetch_add(1, Ordering::SeqCst);
                        let ok = authorized.iter().any(|k| {
                            k.pkcs1v15_verify_prehash::<sha1::Sha1>(&token, &msg.payload)
                                .is_ok()
                        });
                        if ok {
                            break;
                        }
                        token.iter_mut().for_each(|b| *b = b.wrapping_add(1));
                        if send(&mut wr, Message::new(A_AUTH, AUTH_TOKEN, 0, token.to_vec()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    AUTH_RSAPUBLICKEY => {
                        *stats.pubkey.lock().unwrap() = Some(msg.payload.to_vec());
                        if *accept_new_key {
                            tokio::time::sleep(config.accept_delay).await;
                            break;
                        }
                        // The user never answers: keep the client waiting.
                        while read_message_from(&mut rd, false).await.is_ok() {}
                        return;
                    }
                    other => panic!("unexpected AUTH type {other}"),
                }
            }
            if send(&mut wr, connect_msg).await.is_err() {
                return;
            }
        }
    }

    // --- connected: reader, writer and router tasks ---
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Out>();
    let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel::<Ctl>();

    let writer = tokio::spawn(async move {
        while let Some(out) = out_rx.recv().await {
            let bytes = match out {
                Out::Msg(m) => m.encode(old),
                Out::Raw(r) => r,
            };
            if wr.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });
    let reader_stats = stats.clone();
    let reader_ctl = ctl_tx.clone();
    let reader = tokio::spawn(async move {
        loop {
            match read_message_from(&mut rd, old).await {
                Ok(m) => {
                    if reader_ctl.send(Ctl::Incoming(m)).is_err() {
                        break;
                    }
                }
                Err(xrmc_adb::AdbError::Disconnected) => {
                    reader_stats.transport_eof.store(true, Ordering::SeqCst);
                    let _ = reader_ctl.send(Ctl::Eof);
                    break;
                }
                Err(e) => {
                    if e.to_string().contains("checksum") {
                        reader_stats.checksum_errors.fetch_add(1, Ordering::SeqCst);
                    }
                    let _ = reader_ctl.send(Ctl::Eof);
                    break;
                }
            }
        }
    });
    tasks.lock().unwrap().push(writer);
    tasks.lock().unwrap().push(reader);

    let mut streams: HashMap<u32, DevStream> = HashMap::new();
    let mut next_id = 0x1000u32;
    let ctx = Ctx {
        out: out_tx.clone(),
        ctl: ctl_tx.clone(),
        stats: stats.clone(),
        files,
        max_payload,
    };

    while let Some(ctl) = ctl_rx.recv().await {
        match ctl {
            Ctl::Eof => break,
            Ctl::Remove(dev_id) => {
                streams.remove(&dev_id);
            }
            Ctl::Ack(dev_id) => {
                if let Some(s) = streams.get_mut(&dev_id) {
                    s.unacked = false;
                    let _ = out_tx.send(Out::Msg(Message::new(
                        A_OKAY,
                        dev_id,
                        s.client_id,
                        Bytes::new(),
                    )));
                }
            }
            Ctl::Incoming(msg) => match msg.command {
                A_OPEN => {
                    let service = String::from_utf8_lossy(&msg.payload)
                        .trim_end_matches('\0')
                        .to_string();
                    stats.opened.lock().unwrap().push(service.clone());
                    let client_id = msg.arg0;
                    if service == "hang:" {
                        continue;
                    }
                    if service == "disconnect:" {
                        break;
                    }
                    if !is_known_service(&service) {
                        stats.refused_open_reply.fetch_add(1, Ordering::SeqCst);
                        let _ =
                            out_tx.send(Out::Msg(Message::new(A_CLSE, 0, client_id, Bytes::new())));
                        continue;
                    }
                    let dev_id = next_id;
                    next_id += 1;
                    let (in_tx, in_rx) = mpsc::unbounded_channel();
                    let (ok_tx, ok_rx) = mpsc::unbounded_channel();
                    let ack = if service == "blackhole:" {
                        AckPolicy::Never
                    } else {
                        config.ack
                    };
                    streams.insert(
                        dev_id,
                        DevStream {
                            client_id,
                            service: service.clone(),
                            inbound: Some(in_tx),
                            okays: Some(ok_tx),
                            unacked: false,
                            ack,
                        },
                    );
                    let _ = out_tx.send(Out::Msg(Message::new(
                        A_OKAY,
                        dev_id,
                        client_id,
                        Bytes::new(),
                    )));
                    let stream = FakeStream {
                        dev_id,
                        client_id,
                        inbound: in_rx,
                        okays: ok_rx,
                        buf: BytesMut::new(),
                        ctx: ctx.clone(),
                        closed: false,
                    };
                    let handler = tokio::spawn(serve(service, stream));
                    tasks.lock().unwrap().push(handler);
                }
                A_WRTE => {
                    let (client_id, dev_id) = (msg.arg0, msg.arg1);
                    stats
                        .max_write_seen
                        .fetch_max(msg.payload.len(), Ordering::SeqCst);
                    if msg.payload.len() > max_payload {
                        stats.oversize_writes.fetch_add(1, Ordering::SeqCst);
                    }
                    let Some(s) = streams.get_mut(&dev_id) else {
                        continue;
                    };
                    assert_eq!(s.client_id, client_id, "WRTE with wrong client id");
                    if s.unacked {
                        stats.flow_violations.fetch_add(1, Ordering::SeqCst);
                    }
                    s.unacked = true;
                    if let Some(tx) = &s.inbound {
                        let _ = tx.send(msg.payload.clone());
                    }
                    match s.ack {
                        AckPolicy::Immediate => {
                            s.unacked = false;
                            let _ = out_tx.send(Out::Msg(Message::new(
                                A_OKAY,
                                dev_id,
                                client_id,
                                Bytes::new(),
                            )));
                        }
                        AckPolicy::Delayed(d) => {
                            let ctl = ctl_tx.clone();
                            tokio::spawn(async move {
                                tokio::time::sleep(d).await;
                                let _ = ctl.send(Ctl::Ack(dev_id));
                            });
                        }
                        AckPolicy::Never => {}
                    }
                }
                A_OKAY => {
                    if let Some(s) = streams.get(&msg.arg1)
                        && let Some(tx) = &s.okays
                    {
                        let _ = tx.send(());
                    }
                }
                A_CLSE => {
                    let dev_id = msg.arg1;
                    if dev_id == REVERSE_ID && msg.arg0 == 0 {
                        stats.reverse_refused.store(true, Ordering::SeqCst);
                    }
                    if let Some(s) = streams.remove(&dev_id) {
                        stats
                            .closed_by_client
                            .lock()
                            .unwrap()
                            .push(s.service.clone());
                        // adbd answers a CLSE with its own CLSE.
                        let _ = out_tx.send(Out::Msg(Message::new(
                            A_CLSE,
                            dev_id,
                            s.client_id,
                            Bytes::new(),
                        )));
                    }
                }
                other => panic!("fake adbd got unexpected command {}", command_name(other)),
            },
        }
    }
    // Leaving the loop drops the pipe (disconnect) once the other tasks are aborted.
    for t in tasks.lock().unwrap().drain(..) {
        t.abort();
    }
}

const REVERSE_ID: u32 = 0xdead;

fn is_known_service(service: &str) -> bool {
    [
        "echo:",
        "sink:",
        "blackhole:",
        "garbage:",
        "sync:",
        "open-reverse:",
    ]
    .contains(&service)
        || service.starts_with("flood:")
        || service.starts_with("shell,v2,raw:")
        || service.starts_with("shell:")
        || service.starts_with("localabstract:")
}

#[derive(Clone)]
struct Ctx {
    out: mpsc::UnboundedSender<Out>,
    ctl: mpsc::UnboundedSender<Ctl>,
    stats: Arc<Stats>,
    files: Arc<Mutex<HashMap<String, FakeFile>>>,
    max_payload: usize,
}

/// The device end of one stream.
pub struct FakeStream {
    dev_id: u32,
    client_id: u32,
    inbound: mpsc::UnboundedReceiver<Bytes>,
    okays: mpsc::UnboundedReceiver<()>,
    buf: BytesMut,
    ctx: Ctx,
    closed: bool,
}

impl FakeStream {
    async fn recv(&mut self) -> Option<Bytes> {
        if !self.buf.is_empty() {
            return Some(self.buf.split().freeze());
        }
        self.inbound.recv().await
    }

    async fn read_exact(&mut self, n: usize) -> Option<Bytes> {
        while self.buf.len() < n {
            let chunk = self.inbound.recv().await?;
            self.buf.extend_from_slice(&chunk);
        }
        Some(self.buf.split_to(n).freeze())
    }

    /// Send with device-side flow control: one WRTE, then wait for OKAY.
    async fn send(&mut self, data: &[u8]) -> bool {
        for chunk in data.chunks(self.ctx.max_payload) {
            let msg = Message::new(A_WRTE, self.dev_id, self.client_id, chunk.to_vec());
            if self.ctx.out.send(Out::Msg(msg)).is_err() {
                return false;
            }
            if self.okays.recv().await.is_none() {
                return false;
            }
        }
        true
    }

    fn close(&mut self) {
        if !self.closed {
            self.closed = true;
            let _ = self.ctx.out.send(Out::Msg(Message::new(
                A_CLSE,
                self.dev_id,
                self.client_id,
                Bytes::new(),
            )));
            let _ = self.ctx.ctl.send(Ctl::Remove(self.dev_id));
        }
    }
}

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

async fn serve(service: String, mut s: FakeStream) {
    if service == "echo:" || service.starts_with("localabstract:") {
        while let Some(data) = s.recv().await {
            if !s.send(&data).await {
                break;
            }
        }
    } else if service == "sink:" || service == "blackhole:" {
        while let Some(data) = s.recv().await {
            s.ctx
                .stats
                .sink_bytes
                .fetch_add(data.len(), Ordering::SeqCst);
        }
    } else if let Some(n) = service.strip_prefix("flood:") {
        let n: usize = n.parse().unwrap();
        let data = pattern(n);
        for chunk in data.chunks(s.ctx.max_payload) {
            if !s.send(chunk).await {
                return;
            }
            s.ctx
                .stats
                .flood_sent
                .fetch_add(chunk.len(), Ordering::SeqCst);
        }
        s.close();
    } else if service == "open-reverse:" {
        // Device-initiated stream (adb reverse); the client must refuse it with CLSE.
        let open = Message::new(A_OPEN, REVERSE_ID, 0, b"tcp:1234\0".to_vec());
        let _ = s.ctx.out.send(Out::Msg(open));
        while let Some(data) = s.recv().await {
            if !s.send(&data).await {
                break;
            }
        }
    } else if service == "garbage:" {
        let _ = s
            .ctx
            .out
            .send(Out::Raw(b"this is not an adb header!".to_vec()));
    } else if let Some(cmd) = service.strip_prefix("shell,v2,raw:") {
        shell_v2(cmd.to_string(), &mut s).await;
    } else if let Some(cmd) = service.strip_prefix("shell:") {
        shell_legacy(cmd.to_string(), &mut s).await;
    } else if service == "sync:" {
        sync_service(&mut s).await;
    }
}

fn run_command(cmd: &str, stdin: &[u8]) -> (Vec<u8>, Vec<u8>, u8) {
    match cmd {
        "echo hello" => (b"hello\n".to_vec(), vec![], 0),
        "getprop ro.product.model" => (b"Fake Quest\n".to_vec(), vec![], 0),
        "getprop ro.serialno" => (b"FAKE123\n".to_vec(), vec![], 0),
        "fail" => (b"partial\n".to_vec(), b"boom\n".to_vec(), 3),
        "cat" => (stdin.to_vec(), vec![], 0),
        _ if cmd.starts_with("big ") => (pattern(cmd[4..].parse().unwrap()), vec![], 0),
        _ => (
            vec![],
            format!("/system/bin/sh: {cmd}: not found\n").into_bytes(),
            127,
        ),
    }
}

async fn shell_v2(cmd: String, s: &mut FakeStream) {
    let mut stdin = Vec::new();
    // Read the client's packets until it closes stdin (our client always does).
    loop {
        let Some(header) = s.read_exact(5).await else {
            return;
        };
        let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let Some(data) = s.read_exact(len).await else {
            return;
        };
        match header[0] {
            ID_STDIN => stdin.extend_from_slice(&data),
            ID_CLOSE_STDIN => break,
            _ => {}
        }
    }
    let (stdout, stderr, code) = run_command(&cmd, &stdin);
    let mut out = Vec::new();
    // Split stdout into several packets to exercise reassembly.
    for chunk in stdout.chunks(70_000) {
        out.extend(encode_v2_packet(ID_STDOUT, chunk));
    }
    if !stderr.is_empty() {
        out.extend(encode_v2_packet(ID_STDERR, &stderr));
    }
    out.extend(encode_v2_packet(ID_EXIT, &[code]));
    if s.send(&out).await {
        s.close();
    }
}

async fn shell_legacy(cmd: String, s: &mut FakeStream) {
    if cmd == "stream" {
        // A long-running command printing until the host closes the stream.
        let mut i = 0u64;
        loop {
            if !s.send(format!("tick {i}\n").as_bytes()).await {
                s.ctx
                    .stats
                    .commands_ended_by_close
                    .lock()
                    .unwrap()
                    .push(cmd.clone());
                return;
            }
            i += 1;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    let (stdout, stderr, _) = run_command(&cmd, &[]);
    let mut merged = stdout;
    merged.extend(stderr);
    if s.send(&merged).await {
        s.close();
    }
}

async fn sync_service(s: &mut FakeStream) {
    loop {
        let Some(header) = s.read_exact(8).await else {
            return;
        };
        let id = &header[..4];
        let len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
        match id {
            b"SEND" => {
                let Some(spec) = s.read_exact(len).await else {
                    return;
                };
                let spec = String::from_utf8_lossy(&spec).into_owned();
                let (path, mode) = spec.rsplit_once(',').unwrap();
                let mode: u32 = mode.parse().unwrap();
                if path.starts_with("/system/") {
                    let msg = b"couldn't create file: Read-only file system";
                    let mut fail = b"FAIL".to_vec();
                    fail.extend((msg.len() as u32).to_le_bytes());
                    fail.extend(msg);
                    s.send(&fail).await;
                    s.close();
                    return;
                }
                let mut data = Vec::new();
                loop {
                    let Some(h) = s.read_exact(8).await else {
                        return;
                    };
                    let n = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
                    match &h[..4] {
                        b"DATA" => {
                            assert!(n as usize <= 64 * 1024, "DATA chunk too large");
                            let Some(chunk) = s.read_exact(n as usize).await else {
                                return;
                            };
                            data.extend_from_slice(&chunk);
                        }
                        b"DONE" => {
                            s.ctx.files.lock().unwrap().insert(
                                path.to_string(),
                                FakeFile {
                                    mode,
                                    data,
                                    mtime: n,
                                },
                            );
                            s.send(b"OKAY\0\0\0\0").await;
                            break;
                        }
                        other => panic!("unexpected sync id {other:?}"),
                    }
                }
            }
            b"STAT" => {
                let Some(path) = s.read_exact(len).await else {
                    return;
                };
                let path = String::from_utf8_lossy(&path).into_owned();
                let (mode, size, mtime) = match s.ctx.files.lock().unwrap().get(&path) {
                    Some(f) => (f.mode, f.data.len() as u32, f.mtime),
                    None => (0, 0, 0),
                };
                let mut reply = b"STAT".to_vec();
                reply.extend(mode.to_le_bytes());
                reply.extend(size.to_le_bytes());
                reply.extend(mtime.to_le_bytes());
                s.send(&reply).await;
            }
            b"QUIT" => {
                s.close();
                return;
            }
            other => panic!("unexpected sync request {other:?}"),
        }
    }
}

/// The byte pattern used by `flood:` and `big`.
pub fn pattern_bytes(n: usize) -> Vec<u8> {
    pattern(n)
}
