const { EventEmitter } = require('events');
const crypto = require('crypto');
const net = require('net');
const { SCRCPY_VERSION, REMOTE_SERVER_PATH, REMOTE_LIST_SERVER_PATH } = require('./config');

const CODEC_NAMES = { 0x68323634: 'h264', 0x68323635: 'h265', 0x00617631: 'av1' };
const DEVICE_NAME_LENGTH = 64;

const delay = (ms) => new Promise((r) => setTimeout(r, ms));

/** FIFO of Buffers that can hand out exact-size slices without re-concatenating everything. */
class ByteQueue {
  constructor() {
    this.chunks = [];
    this.length = 0;
  }

  push(buf) {
    if (buf.length) {
      this.chunks.push(buf);
      this.length += buf.length;
    }
  }

  take(n) {
    const first = this.chunks[0];
    if (first.length >= n) {
      const out = first.subarray(0, n);
      if (first.length === n) this.chunks.shift();
      else this.chunks[0] = first.subarray(n);
      this.length -= n;
      return out;
    }
    const out = Buffer.allocUnsafe(n);
    let off = 0;
    while (off < n) {
      const c = this.chunks[0];
      const k = Math.min(c.length, n - off);
      c.copy(out, off, 0, k);
      off += k;
      if (k === c.length) this.chunks.shift();
      else this.chunks[0] = c.subarray(k);
    }
    this.length -= n;
    return out;
  }
}

/**
 * Incremental parser for the scrcpy (v4+/v5) video socket, after the dummy byte:
 *   device meta (64 bytes, NUL-padded device name)
 *   codec id (u32 BE)
 *   then repeated 12-byte headers:
 *     MSB set   -> session packet: u32 flags, u32 width, u32 height (no payload)
 *     MSB clear -> media packet: u64 [0|config|key|pts:61], u32 size, followed by `size` bytes
 */
class ScrcpyVideoParser extends EventEmitter {
  constructor() {
    super();
    this.q = new ByteQueue();
    this.state = 'device';
    this.header = null;
  }

  push(chunk) {
    this.q.push(chunk);
    for (;;) {
      if (this.state === 'device') {
        if (this.q.length < DEVICE_NAME_LENGTH) return;
        const raw = this.q.take(DEVICE_NAME_LENGTH);
        const end = raw.indexOf(0);
        this.emit('device', raw.subarray(0, end === -1 ? raw.length : end).toString('utf8'));
        this.state = 'codec';
      } else if (this.state === 'codec') {
        if (this.q.length < 4) return;
        const id = this.q.take(4).readUInt32BE(0);
        if (id === 0 || id === 1) {
          this.emit('error', new Error(id === 1 ? 'The headset could not configure its video encoder.' : 'The headset disabled the video stream.'));
          this.state = 'dead';
          return;
        }
        this.emit('codec', { id, name: CODEC_NAMES[id] || `0x${id.toString(16)}` });
        this.state = 'header';
      } else if (this.state === 'header') {
        if (this.q.length < 12) return;
        const h = this.q.take(12);
        const hi = h.readUInt32BE(0);
        if (hi & 0x80000000) {
          this.emit('session', { width: h.readUInt32BE(4), height: h.readUInt32BE(8), clientResized: !!(hi & 1) });
        } else {
          const ptsFlags = h.readBigUInt64BE(0);
          this.header = {
            config: (ptsFlags & (1n << 62n)) !== 0n,
            key: (ptsFlags & (1n << 61n)) !== 0n,
            pts: Number(ptsFlags & ((1n << 61n) - 1n)),
            size: h.readUInt32BE(8),
          };
          this.state = 'payload';
        }
      } else if (this.state === 'payload') {
        if (this.q.length < this.header.size) return;
        const data = this.q.take(this.header.size);
        const { config, key, pts } = this.header;
        this.header = null;
        this.state = 'header';
        this.emit('packet', { config, key: key && !config, pts, data });
      } else {
        return;
      }
    }
  }
}

/**
 * One live mirroring session for one headset transport:
 * pushes scrcpy-server, opens an adb forward tunnel, starts the server and
 * emits demuxed H.264 packets.
 */
class StreamSession extends EventEmitter {
  constructor(adb, adbSerial, serverPath, opts) {
    super();
    this.adb = adb;
    this.adbSerial = adbSerial;
    this.serverPath = serverPath;
    this.opts = opts;
    this.proc = null;
    this.socket = null;
    this.port = null;
    this.stopped = false;
    this.logLines = [];
    this.bytes = 0;
  }

  log(line) {
    this.logLines.push(line);
    if (this.logLines.length > 40) this.logLines.shift();
    this.emit('log', line);
  }

  lastError() {
    const err = [...this.logLines].reverse().find((l) => /ERROR|Exception|error:/i.test(l));
    return err || this.logLines[this.logLines.length - 1] || null;
  }

  async start() {
    const { adb, adbSerial: serial } = this;
    await adb.run(['push', this.serverPath, REMOTE_SERVER_PATH], { serial, timeout: 30000 });
    if (this.stopped) return;

    const scid = crypto.randomInt(0, 0x7fffffff).toString(16).padStart(8, '0');
    const fwd = await adb.run(['forward', 'tcp:0', `localabstract:scrcpy_${scid}`], { serial, timeout: 10000 });
    this.port = parseInt(fwd.trim(), 10);
    if (!this.port) throw new Error(`adb forward failed: ${fwd.trim()}`);

    const o = this.opts;
    const args = [
      `scid=${scid}`,
      'log_level=info',
      'video=true',
      'audio=false',
      'control=false',
      'tunnel_forward=true',
      'cleanup=true',
      `video_codec=${o.codec || 'h264'}`,
      `max_size=${o.maxSize || 0}`,
      `video_bit_rate=${o.bitRate}`,
      `max_fps=${o.maxFps}`,
      // Frequent key frames so a decoder can recover quickly after packet loss.
      `video_codec_options=${['i-frame-interval=2', ...(o.codecOptions || [])].join(',')}`,
      'send_device_meta=true',
      'send_frame_meta=true',
      'send_dummy_byte=true',
      'send_stream_meta=true',
      ...(o.extraArgs || []),
    ];
    if (o.crop) args.push(`crop=${o.crop}`);
    if (o.encoder) args.push(`video_encoder=${o.encoder}`);

    const cmd = `CLASSPATH=${REMOTE_SERVER_PATH} app_process / com.genymobile.scrcpy.Server ${SCRCPY_VERSION} ${args.join(' ')}`;
    this.proc = adb.spawn(['shell', cmd], serial);
    const onOutput = (buf) => {
      for (const line of buf.toString().split(/\r?\n/)) if (line.trim()) this.log(line.trim());
    };
    this.proc.stdout.on('data', onOutput);
    this.proc.stderr.on('data', onOutput);
    this.proc.on('exit', (code) => {
      this.proc = null;
      if (!this.stopped) this.fail(new Error(this.lastError() || `Server exited (code ${code})`));
    });

    const { sock, rest } = await this.connectWithRetry();
    if (this.stopped) {
      sock.destroy();
      return;
    }
    this.socket = sock;

    const parser = new ScrcpyVideoParser();
    parser.on('device', (name) => this.emit('device', name));
    parser.on('codec', (c) => {
      if (c.name !== (o.codec || 'h264')) this.fail(new Error(`Requested ${o.codec} but the headset sent ${c.name}`));
      else this.emit('codec', c);
    });
    parser.on('session', (s) => this.emit('session', s));
    parser.on('packet', (p) => this.emit('packet', p));
    parser.on('error', (e) => this.fail(e));

    sock.on('data', (chunk) => {
      this.bytes += chunk.length;
      parser.push(chunk);
    });
    sock.on('error', (e) => this.fail(e));
    sock.on('close', () => {
      if (!this.stopped) this.fail(new Error(this.lastError() || 'Stream connection closed'));
    });
    if (rest.length) {
      this.bytes += rest.length;
      parser.push(rest);
    }
    sock.resume();
  }

  /**
   * With a forward tunnel, connecting succeeds as soon as adb listens locally,
   * even before the server is up. The server sends one dummy byte once it has
   * accepted us, so retry until that byte arrives.
   */
  async connectWithRetry() {
    let lastErr;
    for (let attempt = 0; attempt < 60 && !this.stopped; attempt++) {
      try {
        return await this.connectOnce();
      } catch (e) {
        lastErr = e;
        if (!this.proc) break; // server already died, no point retrying
        await delay(attempt < 10 ? 100 : 250);
      }
    }
    throw new Error(this.lastError() || (lastErr && lastErr.message) || 'Could not connect to the headset stream');
  }

  connectOnce() {
    return new Promise((resolve, reject) => {
      const sock = net.connect({ host: '127.0.0.1', port: this.port });
      sock.setNoDelay(true);
      let settled = false;
      const timer = setTimeout(() => fail(new Error('timeout')), 4000);
      const fail = (e) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        sock.destroy();
        reject(e || new Error('closed'));
      };
      sock.once('data', (chunk) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        sock.pause();
        sock.removeAllListeners('error');
        sock.removeAllListeners('close');
        resolve({ sock, rest: chunk.subarray(1) });
      });
      sock.once('error', fail);
      sock.once('close', () => fail());
    });
  }

  fail(err) {
    if (this.stopped) return;
    this.emit('error', err);
    this.stop();
  }

  async stop() {
    if (this.stopped) return;
    this.stopped = true;
    if (this.socket) this.socket.destroy();
    this.socket = null;
    if (this.proc) {
      try {
        this.proc.kill();
      } catch {
        /* ignore */
      }
      this.proc = null;
    }
    if (this.port) {
      const port = this.port;
      this.port = null;
      await this.adb.run(['forward', '--remove', `tcp:${port}`], { serial: this.adbSerial, timeout: 5000 }).catch(() => {});
    }
    this.emit('end');
  }
}

/**
 * Parse scrcpy's "List of video encoders" output. Lines look like:
 *   --video-codec=h265 --video-encoder=c2.qti.hevc.encoder      (hw) [vendor]
 * The (hw)/(sw)/(hybrid) marker only exists on Android 10+; older devices are
 * classified by the well-known software encoder name prefixes.
 */
function parseEncoderList(text) {
  const list = [];
  const re = /--video-codec=(\w+)\s+--video-encoder=(\S+)([^\n]*)/g;
  let m;
  while ((m = re.exec(text))) {
    const [, codec, name, rest] = m;
    let hw;
    if (/\(hw\)|\(hybrid\)/.test(rest)) hw = true;
    else if (/\(sw\)/.test(rest)) hw = false;
    else hw = !/^(c2\.android\.|OMX\.google\.)/i.test(name);
    list.push({ codec, name, hw, alias: /alias for/.test(rest) });
  }
  return list;
}

/** Ask the headset which video encoders it has (runs scrcpy-server in list mode). */
async function listEncoders(adb, serial, serverPath) {
  await adb.run(['push', serverPath, REMOTE_LIST_SERVER_PATH], { serial, timeout: 30000 });
  const out = await adb.shell(
    serial,
    `CLASSPATH=${REMOTE_LIST_SERVER_PATH} app_process / com.genymobile.scrcpy.Server ${SCRCPY_VERSION} list_encoders=true cleanup=true log_level=info`,
    { timeout: 20000 },
  );
  const list = parseEncoderList(out);
  if (!list.length) throw new Error(`No encoders reported: ${out.trim().split('\n').pop()}`);
  return list;
}

module.exports = { StreamSession, ScrcpyVideoParser, ByteQueue, parseEncoderList, listEncoders };
