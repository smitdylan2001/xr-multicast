const { EventEmitter } = require('events');
const crypto = require('crypto');
const fs = require('fs');
const http = require('http');
const path = require('path');

// Optional LAN web server: tablets and phones open http://<pc-ip>:<port> and
// get the same dashboard, talking to this app (the hub) over a WebSocket.

const PIN_MAX_FAILS = 5; // per IP, then locked out
const PIN_LOCK_MS = 5 * 60_000;
const PIN_GLOBAL_FAILS = 30; // from all IPs, then pairing pauses
const PIN_GLOBAL_WINDOW_MS = 10 * 60_000;
const PIN_GLOBAL_LOCK_MS = 60_000;
const MAX_PAIRED = 50;
const MAX_UNAUTHED = 16;
const UNAUTHED_TIMEOUT_MS = 10 * 60_000;
const BACKPRESSURE_BYTES = 4 * 1024 * 1024; // start dropping non-key packets
const BACKPRESSURE_HARD_BYTES = 12 * 1024 * 1024; // drop key frames too
const GOP_MAX_BYTES = 32 * 1024 * 1024;
const GOP_MAX_PACKETS = 900;

const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.mjs': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.webmanifest': 'application/manifest+json; charset=utf-8',
  '.png': 'image/png',
  '.svg': 'image/svg+xml',
  '.ico': 'image/x-icon',
  '.woff2': 'font/woff2',
};

const sha256 = (s) => crypto.createHash('sha256').update(String(s)).digest('hex');

// ------------------------------------------------------------------ auth

/**
 * PIN pairing + long-lived device tokens. `state` is the persisted object
 * ({ pin, clients: [{ id, hash, name, ip, createdAt, lastSeen }] }); only token
 * hashes are stored. `save` is called whenever it changes.
 */
class RemoteAuth {
  constructor(state, { save = () => {}, now = () => Date.now() } = {}) {
    this.state = state;
    this.save = save;
    this.now = now;
    if (!Array.isArray(state.clients)) state.clients = [];
    if (!/^\d{6}$/.test(state.pin || '')) this.newPin();
    this.fails = new Map(); // ip -> { count, until }
    this.globalFails = [];
    this.globalUntil = 0;
  }

  get pin() {
    return this.state.pin;
  }

  newPin() {
    this.state.pin = String(crypto.randomInt(0, 1_000_000)).padStart(6, '0');
    this.save();
    return this.state.pin;
  }

  /** @returns {{ok: true} | {ok: false, error: string, retryAfter?: number}} */
  checkPin(pin, ip) {
    const t = this.now();
    if (t < this.globalUntil) return { ok: false, error: 'Too many wrong PINs. Try again in a minute.', retryAfter: Math.ceil((this.globalUntil - t) / 1000) };
    const f = this.fails.get(ip);
    if (f && f.until > t) return { ok: false, error: 'Too many wrong PINs from this device. Try again later.', retryAfter: Math.ceil((f.until - t) / 1000) };
    const given = Buffer.from(sha256(String(pin == null ? '' : pin).trim()));
    const expected = Buffer.from(sha256(this.state.pin));
    if (crypto.timingSafeEqual(given, expected)) {
      this.fails.delete(ip);
      return { ok: true };
    }
    const entry = f && f.until <= t && f.count >= PIN_MAX_FAILS ? { count: 0, until: 0 } : f || { count: 0, until: 0 };
    entry.count++;
    if (entry.count >= PIN_MAX_FAILS) entry.until = t + PIN_LOCK_MS;
    this.fails.set(ip, entry);
    this.globalFails = this.globalFails.filter((x) => t - x < PIN_GLOBAL_WINDOW_MS);
    this.globalFails.push(t);
    if (this.globalFails.length >= PIN_GLOBAL_FAILS) {
      this.globalUntil = t + PIN_GLOBAL_LOCK_MS;
      this.globalFails = [];
    }
    const left = PIN_MAX_FAILS - entry.count;
    return left > 0
      ? { ok: false, error: `Wrong PIN (${left} attempt${left === 1 ? '' : 's'} left).` }
      : { ok: false, error: 'Too many wrong PINs from this device. Try again later.', retryAfter: Math.ceil(PIN_LOCK_MS / 1000) };
  }

  /** New paired device; returns the plaintext token (only ever sent to that device). */
  issueToken({ name, ip }) {
    const token = crypto.randomBytes(32).toString('base64url');
    const client = { id: crypto.randomBytes(6).toString('hex'), hash: sha256(token), name: String(name || 'Browser').slice(0, 80), ip: ip || null, createdAt: this.now(), lastSeen: this.now() };
    this.state.clients.push(client);
    if (this.state.clients.length > MAX_PAIRED) {
      this.state.clients.sort((a, b) => b.lastSeen - a.lastSeen);
      this.state.clients.length = MAX_PAIRED;
    }
    this.save();
    return { token, client };
  }

  verifyToken(token, ip) {
    if (typeof token !== 'string' || token.length < 20 || token.length > 200) return null;
    const hash = sha256(token);
    const client = this.state.clients.find((c) => c.hash.length === hash.length && crypto.timingSafeEqual(Buffer.from(c.hash), Buffer.from(hash)));
    if (!client) return null;
    client.lastSeen = this.now();
    if (ip) client.ip = ip;
    this.save();
    return client;
  }

  touch(id) {
    const c = this.state.clients.find((x) => x.id === id);
    if (c) {
      c.lastSeen = this.now();
      this.save();
    }
  }

  revoke(id) {
    const before = this.state.clients.length;
    this.state.clients = id === 'all' ? [] : this.state.clients.filter((c) => c.id !== id);
    this.save();
    return before - this.state.clients.length;
  }

  list() {
    return this.state.clients.map(({ id, name, ip, createdAt, lastSeen }) => ({ id, name, ip, createdAt, lastSeen }));
  }
}

// --------------------------------------------------------------- helpers

/** Absolute file path for a URL path inside `root`, or null if it would escape it. */
function resolveStatic(root, urlPath) {
  if (typeof urlPath !== 'string') return null;
  let p;
  try {
    p = decodeURIComponent(urlPath.split('?')[0].split('#')[0]);
  } catch {
    return null;
  }
  if (p.includes('\0') || p.includes('\\')) return null;
  const rootAbs = path.resolve(root);
  const full = path.resolve(rootAbs, `.${path.posix.normalize(`/${p}`)}`);
  if (full !== rootAbs && !full.startsWith(rootAbs + path.sep)) return null;
  const rel = path.relative(rootAbs, full);
  if (rel.split(path.sep).some((s) => s.startsWith('.'))) return null;
  return full;
}

/** Friendly device label from a User-Agent string. */
function deviceName(ua, hint) {
  ua = String(ua || '');
  let device;
  if (/iPad/.test(ua) || (hint === 'iPad' && /Macintosh/.test(ua))) device = 'iPad';
  else if (/iPhone/.test(ua)) device = 'iPhone';
  else if (/Android/.test(ua)) {
    const m = ua.match(/Android [\d.]+;\s*(?:[a-z]{2}[-_][a-zA-Z]{2};\s*)?([^;)]+?)(?:\s+Build\/|\))/);
    device = m && m[1].trim() !== 'K' ? m[1].trim() : /Mobile/.test(ua) ? 'Android phone' : 'Android tablet';
  } else if (/CrOS/.test(ua)) device = 'Chromebook';
  else if (/Macintosh/.test(ua)) device = 'Mac';
  else if (/Windows/.test(ua)) device = 'Windows PC';
  else if (/Linux/.test(ua)) device = 'Linux PC';
  else device = 'Browser';
  let browser = '';
  if (/EdgA?\/|EdgiOS/.test(ua)) browser = 'Edge';
  else if (/SamsungBrowser/.test(ua)) browser = 'Samsung Internet';
  else if (/FxiOS|Firefox\//.test(ua)) browser = 'Firefox';
  else if (/CriOS|Chrome\//.test(ua)) browser = 'Chrome';
  else if (/Safari\//.test(ua)) browser = 'Safari';
  return (browser ? `${device} · ${browser}` : device).slice(0, 80);
}

/** Binary frame for a video packet: u8 flags (1 config, 2 key), u8 idLen, id, f64le pts, data. */
function encodePacket(id, p) {
  const idBuf = Buffer.from(String(id), 'utf8');
  const head = Buffer.alloc(2 + idBuf.length + 8);
  head[0] = (p.config ? 1 : 0) | (p.key ? 2 : 0);
  head[1] = idBuf.length;
  idBuf.copy(head, 2);
  head.writeDoubleLE(Number(p.pts) || 0, 2 + idBuf.length);
  return Buffer.concat([head, p.data]);
}

function decodePacket(buf) {
  const idLen = buf[1];
  return {
    id: buf.subarray(2, 2 + idLen).toString('utf8'),
    config: !!(buf[0] & 1),
    key: !!(buf[0] & 2),
    pts: buf.readDoubleLE(2 + idLen),
    data: buf.subarray(10 + idLen),
  };
}

/** Keep only well-formed decoder support entries ({codec: {any, hw}}). */
function sanitizeSupport(s) {
  if (!s || typeof s !== 'object') return null;
  const out = {};
  for (const [k, v] of Object.entries(s).slice(0, 16)) {
    if (/^[a-z0-9_]{1,16}$/.test(k) && v && typeof v === 'object') out[k] = { any: !!v.any, hw: !!v.hw };
  }
  return Object.keys(out).length ? out : null;
}

function clientIp(req) {
  return String(req.socket.remoteAddress || '').replace(/^::ffff:/, '');
}

const REMOTE_CSP =
  "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; media-src 'self' blob:; connect-src 'self' ws: wss:; manifest-src 'self'";

/** The renderer's index.html, adapted to run in a browser through the remote bridge. */
function remoteIndexHtml(html) {
  const head = [
    '<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover" />',
    '<meta name="theme-color" content="#0b0f17" />',
    '<meta name="mobile-web-app-capable" content="yes" />',
    '<meta name="apple-mobile-web-app-capable" content="yes" />',
    '<meta name="apple-mobile-web-app-status-bar-style" content="black-translucent" />',
    '<meta name="apple-mobile-web-app-title" content="XR MultiCast" />',
    '<link rel="manifest" href="manifest.webmanifest" />',
    '<link rel="icon" type="image/png" href="icon.png" />',
    '<link rel="apple-touch-icon" href="icon.png" />',
  ].join('\n    ');
  let out = html.replace(/(<meta\s+http-equiv="Content-Security-Policy"\s+content=")[^"]*(")/i, `$1${REMOTE_CSP}$2`);
  out = out.replace(/<\/title>/i, `</title>\n    ${head}`);
  out = out.replace(/<html([^>]*)>/i, '<html$1 class="remote">');
  const firstScript = out.search(/<script\s+src="js\//i);
  const bridge = '<script src="js/remote-bridge.js"></script>\n    ';
  return firstScript >= 0 ? out.slice(0, firstScript) + bridge + out.slice(firstScript) : out.replace(/<\/body>/i, `${bridge}</body>`);
}

const MANIFEST = JSON.stringify({
  name: 'XR MultiCast',
  short_name: 'XR MultiCast',
  description: 'View and control VR headset casts from the XR MultiCast hub computer.',
  start_url: './',
  scope: './',
  display: 'standalone',
  orientation: 'any',
  background_color: '#0b0f17',
  theme_color: '#0b0f17',
  icons: [{ src: 'icon.png', sizes: '512x512', type: 'image/png', purpose: 'any' }],
});

// ---------------------------------------------------------------- server

class RemoteServer extends EventEmitter {
  /**
   * @param {object} o
   * @param {import('./store').Store} o.store
   * @param {(method: string, args: any[], ctx: object) => Promise<any>} o.dispatch
   * @param {import('./stream-manager').StreamManager} o.streams
   * @param {import('./device-manager').DeviceManager} o.devices
   * @param {string} o.rendererDir
   * @param {string} o.iconPath
   * @param {() => string[]} o.localIps
   */
  constructor({ store, dispatch, streams, devices, rendererDir, iconPath, localIps }) {
    super();
    this.store = store;
    this.dispatch = dispatch;
    this.streams = streams;
    this.devices = devices;
    this.rendererDir = rendererDir;
    this.iconPath = iconPath;
    this.localIps = localIps;
    if (!store.data.remote || typeof store.data.remote !== 'object') store.data.remote = {};
    this.auth = new RemoteAuth(store.data.remote, { save: () => store.save() });
    this.server = null;
    this.wss = null;
    this.port = null;
    this.error = null;
    this.clients = new Set();
    /** serial -> { config: Buffer|null, gop: Buffer[], bytes } */
    this.gops = new Map();
    this.allowControl = !!store.settings.remoteAllowControl;

    devices.on('change', (snap) => this.broadcast('devices', [snap]));
    devices.on('notice', (n) => this.broadcast('notice', [n]));
    streams.on('notice', (n) => this.broadcast('notice', [n]));
    streams.on('log', (id, line) => this.broadcast('log', [id, line]));
    streams.on('state', (s) => {
      if (['starting', 'reconnecting', 'stopped', 'error'].includes(s.state)) this.gops.delete(s.id);
      this.broadcast('state', [s]);
    });
    streams.on('packet', (id, p) => this.onPacket(id, p));
  }

  get running() {
    return !!this.server && this.server.listening;
  }

  /** Start, stop or restart to match the settings. */
  async applySettings() {
    const s = this.store.settings;
    const port = Number(s.remotePort) || 8787;
    const allow = !!s.remoteAllowControl;
    if (allow !== this.allowControl) {
      this.allowControl = allow;
      for (const c of this.clients) if (c.authed) this.sendJson(c, { t: 'access', canControl: allow });
    }
    if (!s.remoteEnabled) await this.stop();
    else if (!this.server || this.port !== port) {
      await this.stop();
      await this.start(port);
    }
    this.emitStatus();
  }

  start(port) {
    const { WebSocketServer } = require('ws');
    this.error = null;
    return new Promise((resolve) => {
      const server = http.createServer((req, res) => this.handleHttp(req, res));
      const wss = new WebSocketServer({ noServer: true, maxPayload: 64 * 1024 });
      server.on('upgrade', (req, socket, head) => {
        if ((req.url || '').split('?')[0] !== '/ws' || !this.originOk(req)) {
          socket.write('HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n');
          socket.destroy();
          return;
        }
        const unauthed = [...this.clients].filter((c) => !c.authed).length;
        if (unauthed >= MAX_UNAUTHED) {
          socket.write('HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\n\r\n');
          socket.destroy();
          return;
        }
        wss.handleUpgrade(req, socket, head, (ws) => this.onConnection(ws, req));
      });
      server.on('error', (e) => {
        this.error = e.code === 'EADDRINUSE' ? `Port ${port} is already in use.` : e.message;
        this.server = null;
        this.wss = null;
        this.emitStatus();
        resolve(false);
      });
      server.listen(port, '0.0.0.0', () => {
        this.port = port;
        this.emitStatus();
        resolve(true);
      });
      this.server = server;
      this.wss = wss;
      this.port = port;
      this.pingTimer = setInterval(() => this.heartbeat(), 20_000);
    });
  }

  async stop() {
    clearInterval(this.pingTimer);
    for (const c of this.clients) {
      try {
        c.ws.terminate();
      } catch {
        /* ignore */
      }
      this.dropClient(c);
    }
    this.gops.clear();
    const server = this.server;
    this.server = null;
    if (this.wss) this.wss.close();
    this.wss = null;
    if (server) {
      await new Promise((r) => {
        server.close(() => r());
        if (server.closeAllConnections) server.closeAllConnections();
        setTimeout(r, 1000);
      });
    }
    this.port = null;
  }

  originOk(req) {
    const origin = req.headers.origin;
    if (!origin) return true;
    try {
      return new URL(origin).host === req.headers.host;
    } catch {
      return false;
    }
  }

  // ---------------------------------------------------------------- http

  handleHttp(req, res) {
    const send = (code, body, headers = {}) => {
      res.writeHead(code, {
        'X-Content-Type-Options': 'nosniff',
        'Referrer-Policy': 'no-referrer',
        'X-Frame-Options': 'DENY',
        'Cache-Control': 'no-store',
        ...headers,
      });
      res.end(req.method === 'HEAD' ? undefined : body);
    };
    if (req.method !== 'GET' && req.method !== 'HEAD') return send(405, 'Method not allowed', { Allow: 'GET, HEAD' });
    const url = (req.url || '/').split('?')[0];
    if (url === '/' || url === '/index.html') {
      fs.readFile(path.join(this.rendererDir, 'index.html'), 'utf8', (err, html) => {
        if (err) return send(500, 'index.html missing');
        send(200, remoteIndexHtml(html), { 'Content-Type': MIME['.html'], 'Cache-Control': 'no-cache' });
      });
      return;
    }
    if (url === '/manifest.webmanifest') return send(200, MANIFEST, { 'Content-Type': MIME['.webmanifest'], 'Cache-Control': 'no-cache' });
    if (url === '/icon.png' || url === '/apple-touch-icon.png' || url === '/favicon.ico') return this.sendFile(req, res, this.iconPath, 'public, max-age=86400');
    if (url === '/ws') return send(426, 'WebSocket only');
    const file = resolveStatic(this.rendererDir, url);
    if (!file || !MIME[path.extname(file)] || path.basename(file) === 'index.html') return send(404, 'Not found');
    this.sendFile(req, res, file, 'no-cache');
  }

  sendFile(req, res, file, cacheControl) {
    fs.stat(file, (err, st) => {
      if (err || !st.isFile()) {
        res.writeHead(404, { 'Content-Type': 'text/plain', 'X-Content-Type-Options': 'nosniff' });
        res.end('Not found');
        return;
      }
      const etag = `"${st.size.toString(16)}-${Math.floor(st.mtimeMs).toString(16)}"`;
      const headers = {
        'Content-Type': MIME[path.extname(file)] || 'application/octet-stream',
        'Cache-Control': cacheControl,
        ETag: etag,
        'Last-Modified': st.mtime.toUTCString(),
        'X-Content-Type-Options': 'nosniff',
        'Referrer-Policy': 'no-referrer',
      };
      if (req.headers['if-none-match'] === etag) {
        res.writeHead(304, headers);
        res.end();
        return;
      }
      res.writeHead(200, { ...headers, 'Content-Length': st.size });
      if (req.method === 'HEAD') return res.end();
      fs.createReadStream(file).on('error', () => res.destroy()).pipe(res);
    });
  }

  // ----------------------------------------------------------- websocket

  onConnection(ws, req) {
    const c = {
      id: crypto.randomBytes(8).toString('hex'),
      ws,
      ip: clientIp(req),
      ua: String(req.headers['user-agent'] || '').slice(0, 400),
      authed: false,
      record: null,
      subs: new Set(),
      dropping: new Set(),
      support: null,
      alive: true,
      since: Date.now(),
    };
    this.clients.add(c);
    c.authTimer = setTimeout(() => {
      if (!c.authed) ws.close(4001, 'Pairing timed out');
    }, UNAUTHED_TIMEOUT_MS);
    ws.on('pong', () => (c.alive = true));
    ws.on('message', (data, isBinary) => {
      if (isBinary) return ws.close(1003, 'Binary not accepted');
      let msg;
      try {
        msg = JSON.parse(String(data));
      } catch {
        return ws.close(1007, 'Bad JSON');
      }
      if (!msg || typeof msg !== 'object') return;
      this.onMessage(c, msg).catch((e) => this.emit('log', `remote: ${e.message}`));
    });
    ws.on('close', () => this.dropClient(c));
    ws.on('error', () => this.dropClient(c));
    this.sendJson(c, { t: 'hello', needAuth: true });
  }

  dropClient(c) {
    if (!this.clients.has(c)) return;
    this.clients.delete(c);
    clearTimeout(c.authTimer);
    if (c.support) this.streams.setViewerSupport(c.id, null);
    c.support = null;
    this.emitStatus();
  }

  heartbeat() {
    for (const c of this.clients) {
      if (!c.alive) {
        c.ws.terminate();
        this.dropClient(c);
        continue;
      }
      c.alive = false;
      try {
        c.ws.ping();
      } catch {
        /* ignore */
      }
    }
  }

  sendJson(c, obj) {
    if (c.ws.readyState === 1) c.ws.send(JSON.stringify(obj));
  }

  async onMessage(c, msg) {
    switch (msg.t) {
      case 'pair': {
        if (c.authed) return;
        const r = this.auth.checkPin(msg.pin, c.ip);
        if (!r.ok) return this.sendJson(c, { t: 'pair-failed', error: r.error, retryAfter: r.retryAfter || 0 });
        const { token, client } = this.auth.issueToken({ name: deviceName(c.ua, msg.hint), ip: c.ip });
        this.emit('notice', { level: 'info', message: `Paired a remote viewer: ${client.name} (${c.ip}).` });
        this.sendJson(c, { t: 'paired', token });
        return this.ready(c, client, msg.support);
      }
      case 'auth': {
        if (c.authed) return;
        const client = this.auth.verifyToken(msg.token, c.ip);
        if (!client) return this.sendJson(c, { t: 'auth-failed', error: 'This device is no longer paired. Enter the PIN shown on the hub.' });
        return this.ready(c, client, msg.support);
      }
      default:
        break;
    }
    if (!c.authed) {
      this.sendJson(c, { t: 'error', id: msg.id, error: 'Not paired.' });
      return;
    }
    switch (msg.t) {
      case 'call': {
        const id = Number.isInteger(msg.id) ? msg.id : null;
        let reply;
        try {
          if (typeof msg.method !== 'string' || msg.method.length > 64) throw new Error('Bad method');
          const args = Array.isArray(msg.args) ? msg.args : [];
          const value = await this.dispatch(msg.method, args, { remote: true, canControl: this.allowControl, clientId: c.id, ip: c.ip });
          reply = { t: 'res', id, ok: true, value: value === undefined ? null : value };
        } catch (e) {
          reply = { t: 'res', id, ok: false, error: e.message || String(e) };
        }
        this.sendJson(c, reply);
        break;
      }
      case 'subscribe':
        this.subscribe(c, msg.ids);
        break;
      case 'support':
        this.setSupport(c, msg.support);
        break;
      case 'unpair':
        this.auth.revoke(c.record.id);
        c.ws.close(4003, 'Unpaired');
        this.emitStatus();
        break;
      default:
        break;
    }
  }

  ready(c, record, support) {
    c.authed = true;
    c.record = record;
    clearTimeout(c.authTimer);
    this.setSupport(c, support);
    this.sendJson(c, { t: 'ready', clientId: c.id, canControl: this.allowControl, name: record.name });
    this.emitStatus();
  }

  setSupport(c, support) {
    // Control-only clients (e.g. hardware button panels) don't watch video and must not constrain codecs.
    if (support === false) {
      if (c.support) this.streams.setViewerSupport(c.id, null);
      c.support = null;
      return;
    }
    // Until told otherwise assume the lowest common denominator (H.264).
    const s = sanitizeSupport(support) || { h264: { any: true, hw: true } };
    c.support = s;
    this.streams.setViewerSupport(c.id, s);
  }

  subscribe(c, ids) {
    if (!Array.isArray(ids)) return;
    const next = new Set(ids.filter((x) => typeof x === 'string' && x.length <= 128).slice(0, 64));
    for (const id of next) {
      if (c.subs.has(id)) continue;
      const g = this.gops.get(id);
      c.dropping.delete(id);
      if (!g) continue;
      if (g.config) c.ws.send(g.config);
      if (c.ws.bufferedAmount > BACKPRESSURE_BYTES) c.dropping.add(id);
      else for (const f of g.gop) c.ws.send(f);
    }
    c.subs = next;
  }

  onPacket(id, p) {
    if (!this.running) return;
    const frame = encodePacket(id, p);
    let g = this.gops.get(id);
    if (!g) this.gops.set(id, (g = { config: null, gop: [], bytes: 0 }));
    if (p.config) {
      g.config = frame;
      g.gop = [];
      g.bytes = 0;
    } else if (p.key) {
      g.gop = [frame];
      g.bytes = frame.length;
    } else if (g.gop.length) {
      g.gop.push(frame);
      g.bytes += frame.length;
      if (g.bytes > GOP_MAX_BYTES || g.gop.length > GOP_MAX_PACKETS) {
        g.gop = [];
        g.bytes = 0;
      }
    }
    for (const c of this.clients) {
      if (!c.authed || !c.subs.has(id) || c.ws.readyState !== 1) continue;
      const buffered = c.ws.bufferedAmount;
      if (!p.config) {
        if (p.key) {
          if (buffered > BACKPRESSURE_HARD_BYTES) {
            c.dropping.add(id);
            continue;
          }
          c.dropping.delete(id);
        } else if (c.dropping.has(id) || buffered > BACKPRESSURE_BYTES) {
          c.dropping.add(id);
          continue;
        }
      }
      c.ws.send(frame);
    }
  }

  broadcast(event, args) {
    if (!this.running) return;
    const msg = JSON.stringify({ t: 'event', event, args });
    for (const c of this.clients) if (c.authed && c.ws.readyState === 1) c.ws.send(msg);
  }

  // -------------------------------------------------------- hub controls

  newPin() {
    this.auth.newPin();
    this.emitStatus();
    return this.auth.pin;
  }

  revoke(id) {
    if (typeof id !== 'string') throw new Error('Bad id');
    const n = this.auth.revoke(id);
    for (const c of [...this.clients]) {
      if (c.authed && (id === 'all' || c.record.id === id)) {
        this.sendJson(c, { t: 'revoked' });
        c.ws.close(4003, 'Revoked');
        this.dropClient(c);
      }
    }
    this.emitStatus();
    return n;
  }

  urls() {
    const port = this.port || Number(this.store.settings.remotePort) || 8787;
    return this.localIps().map((ip) => `http://${ip}:${port}`);
  }

  status() {
    const s = this.store.settings;
    const viewers = [...this.clients].filter((c) => c.authed);
    return {
      enabled: !!s.remoteEnabled,
      running: this.running,
      port: this.port || Number(s.remotePort) || 8787,
      allowControl: !!s.remoteAllowControl,
      error: this.error,
      viewers: viewers.length,
    };
  }

  /** Everything the hub's settings dialog shows. */
  async info() {
    const QRCode = require('qrcode');
    const urls = this.urls();
    const qrs = {};
    for (const u of urls) qrs[u] = await QRCode.toString(u, { type: 'svg', margin: 1, errorCorrectionLevel: 'M', color: { dark: '#000000', light: '#ffffff' } });
    const connected = new Map();
    for (const c of this.clients) if (c.authed) connected.set(c.record.id, c.ip);
    return {
      ...this.status(),
      urls,
      qrs,
      pin: this.auth.pin,
      paired: this.auth.list().map((p) => ({ ...p, connected: connected.has(p.id), ip: connected.get(p.id) || p.ip })),
    };
  }

  emitStatus() {
    this.emit('status', this.status());
  }
}

module.exports = { RemoteServer, RemoteAuth, resolveStatic, deviceName, encodePacket, decodePacket, sanitizeSupport, remoteIndexHtml };
