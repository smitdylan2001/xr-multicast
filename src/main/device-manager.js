const { EventEmitter } = require('events');
const { BRANDS, detectBrand, detectVariant, friendlyModel } = require('../shared/brands');
const { DEFAULT_ADB_TCP_PORT } = require('./config');
const { scanForAdb, probe } = require('./discovery');

const POLL_MS = 2000;
const INFO_REFRESH_MS = 30000;
const RECONNECT_MS = 15000;

const HEADSET_COLORS = ['#F97316', '#22C55E', '#3B82F6', '#EAB308', '#EC4899', '#14B8A6', '#A855F7', '#EF4444', '#84CC16', '#06B6D4', '#F43F5E', '#6366F1'];

const delay = (ms) => new Promise((r) => setTimeout(r, ms));

function hostOf(adbSerial) {
  const m = adbSerial.match(/^([\d.]+):(\d+)$/);
  return m ? { ip: m[1], port: Number(m[2]) } : null;
}

function parseAddress(address) {
  const m = String(address).trim().match(/^([\w.\-]+)(?::(\d+))?$/);
  if (!m) throw new Error(`Invalid address "${address}". Use e.g. 192.168.1.50 or 192.168.1.50:5555`);
  return { host: m[1], port: m[2] ? Number(m[2]) : DEFAULT_ADB_TCP_PORT };
}

class DeviceManager extends EventEmitter {
  constructor(adb, store) {
    super();
    this.adb = adb;
    this.store = store;
    /** hardware serial -> live headset */
    this.headsets = new Map();
    /** adb serial -> hardware serial (identified transports) */
    this.transports = new Map();
    /** adb serial -> { state, network, props } for unauthorized / offline / connecting transports */
    this.pending = new Map();
    this.identifying = new Set();
    this.adbError = null;
    this._lastSnapshot = '';
    this._timers = [];
    this._polling = false;
  }

  async startAdbServer() {
    if (!this.adb.bin) {
      this.adbError = null; // "not installed" is shown separately, with an install button
      this.emitChange(true);
      return;
    }
    try {
      await this.adb.startServer();
      this.adbError = null;
    } catch (e) {
      this.adbError = `Could not start adb (${this.adb.bin}): ${e.message}`;
      this.emitChange(true);
    }
  }

  /** Call after adb was installed or its path changed. */
  async restartAdb() {
    await this.startAdbServer();
    await this.poll();
    this.autoReconnect();
  }

  async start() {
    await this.startAdbServer();
    await this.poll();
    this._timers.push(setInterval(() => this.poll(), POLL_MS));
    this._timers.push(setInterval(() => this.refreshAllInfo(), INFO_REFRESH_MS));
    this._timers.push(setInterval(() => this.autoReconnect(), RECONNECT_MS));
    this.autoReconnect();
  }

  stop() {
    this._timers.forEach(clearInterval);
    this._timers = [];
  }

  notice(level, message) {
    this.emit('notice', { level, message });
  }

  // ---------------------------------------------------------------- polling

  async poll() {
    if (this._polling || !this.adb.bin) return;
    this._polling = true;
    try {
      const list = await this.adb.devices();
      this.adbError = null;
      const seen = new Set();
      for (const d of list) {
        seen.add(d.serial);
        if (d.state === 'device') {
          this.pending.delete(d.serial);
          if (!this.transports.has(d.serial) && !this.identifying.has(d.serial)) {
            this.identifying.add(d.serial);
            this.pending.set(d.serial, { state: 'identifying', network: d.network, props: d.props });
            this.identify(d)
              .catch((e) => {
                this.pending.set(d.serial, { state: 'error', network: d.network, props: d.props, error: e.message });
              })
              .finally(() => {
                this.identifying.delete(d.serial);
                this.emitChange();
              });
          }
        } else {
          if (this.transports.has(d.serial)) this.detachTransport(d.serial);
          const prev = this.pending.get(d.serial);
          if (!prev && d.state === 'unauthorized') {
            this.notice('warn', `Headset ${d.props.model || d.serial} is waiting for permission — put it on and accept "Allow USB debugging".`);
          }
          this.pending.set(d.serial, { state: d.state, network: d.network, props: d.props });
        }
      }
      for (const adbSerial of [...this.transports.keys()]) {
        if (!seen.has(adbSerial)) this.detachTransport(adbSerial);
      }
      for (const adbSerial of [...this.pending.keys()]) {
        if (!seen.has(adbSerial) && !this.identifying.has(adbSerial)) this.pending.delete(adbSerial);
      }
    } catch (e) {
      this.adbError = e.message;
    } finally {
      this._polling = false;
      this.emitChange();
    }
  }

  async identify(d) {
    const out = await this.adb.shell(
      d.serial,
      'getprop ro.product.manufacturer; getprop ro.product.brand; getprop ro.product.model; getprop ro.product.device; ' +
        'getprop ro.product.name; getprop ro.serialno; getprop ro.build.version.release; ' +
        "pm list features 2>/dev/null | grep -i xr; echo '#end'",
      { timeout: 12000 },
    );
    const lines = out.split(/\r?\n/).map((l) => l.trim());
    const [manufacturer, brand, model, device, product, serialno, androidVersion, ...rest] = lines;
    const features = rest.filter((l) => l.startsWith('feature:')).map((l) => l.slice(8));
    const info = { manufacturer, brand, model, device, product, androidVersion, features };
    const serial = serialno || d.serial;
    const brandId = detectBrand(info);

    let hs = this.headsets.get(serial);
    const isNew = !hs;
    if (!hs) {
      hs = { serial, transports: new Map(), battery: null, ip: null };
      this.headsets.set(serial, hs);
    }
    hs.info = info;
    hs.brand = brandId;
    hs.model = friendlyModel(brandId, info);
    hs.transports.set(d.serial, { network: d.network });
    this.transports.set(d.serial, serial);
    this.pending.delete(d.serial);

    const host = hostOf(d.serial);
    if (host) hs.ip = host.ip;

    const rec = this.store.getHeadset(serial);
    const label = rec ? rec.name : `${BRANDS[brandId].name} ${hs.model}`;
    if (rec) {
      const patch = { brand: brandId, model: hs.model, lastSeen: Date.now() };
      if (host) Object.assign(patch, { ip: host.ip, port: host.port });
      this.store.upsertHeadset(serial, patch);
      if (isNew || d.network) this.notice('ok', `${label} connected via ${d.network ? 'Wi-Fi' : 'USB'}.`);
      if (this.store.settings.autoCastOnConnect) this.emit('auto-cast', serial);
    } else if (!d.network) {
      this.notice('info', `New headset on USB: ${label}. Click "Verify" to add it.`);
    } else {
      this.notice('warn', `Unverified headset found on the network: ${label}. Connect it via USB once to verify it.`);
    }

    await this.refreshInfo(serial).catch(() => {});
    this.emitChange();
  }

  detachTransport(adbSerial) {
    const serial = this.transports.get(adbSerial);
    this.transports.delete(adbSerial);
    if (!serial) return;
    const hs = this.headsets.get(serial);
    if (!hs) return;
    hs.transports.delete(adbSerial);
    this.emit('transport-lost', { serial, adbSerial });
    if (hs.transports.size === 0) {
      this.headsets.delete(serial);
      const rec = this.store.getHeadset(serial);
      this.notice('info', `${rec ? rec.name : hs.model} disconnected.`);
    }
  }

  // ------------------------------------------------------------ device info

  usbSerial(serial) {
    const hs = this.headsets.get(serial);
    if (!hs) return null;
    for (const [s, t] of hs.transports) if (!t.network) return s;
    return null;
  }

  wifiSerial(serial) {
    const hs = this.headsets.get(serial);
    if (!hs) return null;
    for (const [s, t] of hs.transports) if (t.network) return s;
    return null;
  }

  /** Pick the adb transport for a headset. 'auto' prefers USB (lower latency, no Wi-Fi load). */
  pickTransport(serial, preference = 'auto') {
    const usb = this.usbSerial(serial);
    const wifi = this.wifiSerial(serial);
    if (preference === 'usb') return usb ? { adbSerial: usb, kind: 'usb' } : null;
    if (preference === 'wifi') return wifi ? { adbSerial: wifi, kind: 'wifi' } : null;
    if (usb) return { adbSerial: usb, kind: 'usb' };
    if (wifi) return { adbSerial: wifi, kind: 'wifi' };
    return null;
  }

  async fetchIp(adbSerial) {
    const out = await this.adb.shell(adbSerial, 'ip -o -4 addr show 2>/dev/null', { timeout: 6000 });
    const entries = [];
    for (const line of out.split(/\r?\n/)) {
      const m = line.match(/^\d+:\s+(\S+)\s+inet\s+([\d.]+)\//);
      if (m && m[1] !== 'lo' && !m[2].startsWith('127.')) entries.push({ iface: m[1], ip: m[2] });
    }
    const wlan = entries.find((e) => /wlan|wifi/i.test(e.iface));
    return (wlan || entries[0] || {}).ip || null;
  }

  async refreshInfo(serial) {
    const hs = this.headsets.get(serial);
    if (!hs) return;
    const t = this.pickTransport(serial);
    if (!t) return;
    const out = await this.adb.shell(t.adbSerial, 'dumpsys battery', { timeout: 6000 });
    const level = out.match(/^\s*level:\s*(\d+)/m);
    const status = out.match(/^\s*status:\s*(\d+)/m);
    const powered = /(AC|USB|Wireless) powered:\s*true/i.test(out);
    if (level) {
      hs.battery = {
        level: Number(level[1]),
        charging: (status && ['2', '5'].includes(status[1])) || powered,
      };
    }
    if (!hostOf(t.adbSerial)) {
      const ip = await this.fetchIp(t.adbSerial).catch(() => null);
      if (ip) {
        hs.ip = ip;
        if (this.store.getHeadset(serial)) this.store.upsertHeadset(serial, { ip });
      }
    }
  }

  async refreshAllInfo() {
    await Promise.allSettled([...this.headsets.keys()].map((s) => this.refreshInfo(s)));
    this.emitChange();
  }

  // ---------------------------------------------------------- user actions

  async verify(serial, { name, color, enableWireless } = {}) {
    const hs = this.headsets.get(serial);
    if (!hs) throw new Error('Headset is not connected.');
    const usb = this.usbSerial(serial);
    if (!usb) throw new Error('Verification requires a USB connection. Plug the headset in with a USB data cable.');
    const echo = await this.adb.shell(usb, 'echo xrmc-verified', { timeout: 6000 });
    if (!echo.includes('xrmc-verified')) throw new Error('The headset did not respond over USB.');

    const settings = this.store.settings;
    const index = settings.nextIndex || 1;
    this.store.updateSettings({ nextIndex: index + 1 });
    const used = new Set(this.store.allHeadsets().map((h) => h.color));
    const autoColor = HEADSET_COLORS.find((c) => !used.has(c)) || HEADSET_COLORS[index % HEADSET_COLORS.length];

    this.store.upsertHeadset(serial, {
      name: (name || '').trim() || `${BRANDS[hs.brand].name} ${hs.model} #${index}`,
      color: color || autoColor,
      index,
      brand: hs.brand,
      model: hs.model,
      modelRaw: hs.info.model,
      manufacturer: hs.info.manufacturer,
      verifiedAt: Date.now(),
      lastSeen: Date.now(),
      ip: hs.ip,
      port: DEFAULT_ADB_TCP_PORT,
    });
    this.notice('ok', `Verified ${this.store.getHeadset(serial).name}.`);
    this.emitChange(true);

    if (enableWireless) {
      try {
        await this.enableWireless(serial);
      } catch (e) {
        this.notice('warn', `Verified, but Wi-Fi casting could not be enabled: ${e.message}`);
      }
    }
    return this.store.getHeadset(serial);
  }

  async enableWireless(serial) {
    const usb = this.usbSerial(serial);
    if (!usb) throw new Error('Connect the headset via USB first.');
    const hs = this.headsets.get(serial);
    const ip = (await this.fetchIp(usb).catch(() => null)) || hs.ip;
    if (!ip) throw new Error('The headset does not seem to be connected to Wi-Fi.');
    const port = DEFAULT_ADB_TCP_PORT;
    await this.adb.run(['tcpip', String(port)], { serial: usb, timeout: 10000 });
    let lastErr;
    for (let attempt = 0; attempt < 6; attempt++) {
      await delay(attempt === 0 ? 2000 : 1200);
      try {
        await this.adb.connect(`${ip}:${port}`);
        lastErr = null;
        break;
      } catch (e) {
        lastErr = e;
      }
    }
    if (lastErr) throw new Error(`Could not reach ${ip}:${port} — make sure this computer and the headset are on the same network. (${lastErr.message})`);
    if (this.store.getHeadset(serial)) this.store.upsertHeadset(serial, { ip, port, wireless: true });
    this.notice('ok', `Wi-Fi casting enabled at ${ip}. You can unplug the USB cable.`);
    await this.poll();
    return { ip, port };
  }

  async disconnectWireless(serial) {
    const wifi = this.wifiSerial(serial);
    if (wifi) await this.adb.disconnect(wifi);
    if (this.store.getHeadset(serial)) this.store.upsertHeadset(serial, { wireless: false });
    await this.poll();
  }

  async connectAddress(address) {
    const { host, port } = parseAddress(address);
    await this.adb.connect(`${host}:${port}`);
    await this.poll();
  }

  async forget(serial) {
    const wifi = this.wifiSerial(serial);
    if (wifi) await this.adb.disconnect(wifi);
    this.store.removeHeadset(serial);
    await this.poll();
    this.emitChange(true);
  }

  update(serial, patch) {
    if (!this.store.getHeadset(serial)) throw new Error('Only verified headsets can be edited.');
    const allowed = {};
    for (const k of ['name', 'color', 'calibration', 'transport', 'viewMode']) {
      if (k in patch) allowed[k] = patch[k];
    }
    this.store.upsertHeadset(serial, allowed);
    this.emitChange(true);
    return this.store.getHeadset(serial);
  }

  connectedWifiIps() {
    const ips = new Set();
    for (const s of this.transports.keys()) {
      const h = hostOf(s);
      if (h) ips.add(h.ip);
    }
    for (const s of this.pending.keys()) {
      const h = hostOf(s);
      if (h) ips.add(h.ip);
    }
    return ips;
  }

  async autoReconnect() {
    if (!this.store.settings.autoReconnectWifi || !this.adb.bin) return;
    const connected = this.connectedWifiIps();
    const targets = this.store
      .allHeadsets()
      .filter((r) => r.ip && r.wireless !== false && !connected.has(r.ip) && !this.wifiSerial(r.serial));
    await Promise.allSettled(
      targets.map(async (r) => {
        const port = r.port || DEFAULT_ADB_TCP_PORT;
        if (await probe(r.ip, port, 700)) await this.adb.connect(`${r.ip}:${port}`, 5000);
      }),
    );
    if (targets.length) await this.poll();
  }

  /** Find headsets on the LAN: saved addresses, mDNS-advertised adb, and a subnet scan of the adb port. */
  async scanNetwork() {
    const port = Number(this.store.settings.scanPort) || DEFAULT_ADB_TCP_PORT;
    const before = new Set(this.transports.keys());
    const tried = new Set();
    const attempts = [];
    const tryConnect = (addr) => {
      if (tried.has(addr)) return;
      tried.add(addr);
      attempts.push(this.adb.connect(addr, 5000).catch(() => null));
    };

    const connected = this.connectedWifiIps();
    for (const r of this.store.allHeadsets()) {
      if (r.ip && !connected.has(r.ip)) tryConnect(`${r.ip}:${r.port || DEFAULT_ADB_TCP_PORT}`);
    }
    for (const svc of await this.adb.mdnsServices()) {
      if (!connected.has(svc.ip)) tryConnect(`${svc.ip}:${svc.port}`);
    }
    const open = await scanForAdb(port);
    for (const ip of open) {
      if (!connected.has(ip)) tryConnect(`${ip}:${port}`);
    }
    await Promise.all(attempts);
    await this.poll();
    // Give identification a moment to finish for fresh transports.
    for (let i = 0; i < 20 && this.identifying.size; i++) await delay(250);

    const fresh = [...this.transports.keys()].filter((s) => !before.has(s));
    const unverified = fresh.filter((s) => !this.store.getHeadset(this.transports.get(s))).length;
    const pendingAuth = [...this.pending.values()].filter((p) => p.network && p.state === 'unauthorized').length;
    return { probed: open.length, connected: fresh.length, unverified, pendingAuth };
  }

  // -------------------------------------------------------------- snapshot

  snapshot() {
    const settings = this.store.settings;
    const out = [];
    const ids = new Set([...this.headsets.keys(), ...this.store.allHeadsets().map((r) => r.serial)]);
    for (const id of ids) {
      const live = this.headsets.get(id);
      const rec = this.store.getHeadset(id);
      const brand = (live && live.brand) || (rec && rec.brand) || 'android';
      const usb = live ? this.usbSerial(id) : null;
      const wifi = live ? this.wifiSerial(id) : null;
      const online = !!(usb || wifi);
      const model = (live && live.model) || (rec && rec.model) || 'Unknown';
      out.push({
        id,
        serial: id,
        trusted: !!rec,
        index: rec ? rec.index : null,
        name: rec ? rec.name : `${BRANDS[brand].name} ${model}`,
        color: rec ? rec.color : BRANDS[brand].color,
        brand,
        model,
        modelRaw: live ? live.info.model : rec && rec.modelRaw,
        variant: detectVariant(brand, live ? live.info : { model: rec && rec.modelRaw }),
        manufacturer: live ? live.info.manufacturer : rec && rec.manufacturer,
        androidVersion: live ? live.info.androidVersion : null,
        usb,
        wifi,
        ip: (live && live.ip) || (rec && rec.ip) || null,
        battery: live ? live.battery : null,
        status: online ? 'online' : 'offline',
        canCast: online && (!!rec || !settings.requireUsbVerification),
        calibration: rec ? rec.calibration || null : null,
        transport: (rec && rec.transport) || 'auto',
        viewMode: (rec && rec.viewMode) || null,
        verifiedAt: rec ? rec.verifiedAt : null,
        lastSeen: rec ? rec.lastSeen : null,
      });
    }
    for (const [adbSerial, p] of this.pending) {
      if (this.transports.has(adbSerial)) continue;
      const guess = detectBrand({ model: p.props.model, device: p.props.device, product: p.props.product, features: [] });
      out.push({
        id: `pending:${adbSerial}`,
        adbSerial,
        pending: true,
        status: p.state,
        network: p.network,
        error: p.error || null,
        brand: guess,
        color: BRANDS[guess].color,
        name: (p.props.model || adbSerial).replace(/_/g, ' '),
        model: (p.props.model || '').replace(/_/g, ' '),
        trusted: false,
        canCast: false,
      });
    }
    out.sort((a, b) => {
      const rank = (h) => (h.pending ? 0 : h.status === 'online' ? (h.trusted ? 1 : 0.5) : 2);
      return rank(a) - rank(b) || (a.index || 999) - (b.index || 999) || a.name.localeCompare(b.name);
    });
    return { headsets: out, adbError: this.adbError };
  }

  emitChange(force = false) {
    const snap = this.snapshot();
    const json = JSON.stringify(snap);
    if (!force && json === this._lastSnapshot) return;
    this._lastSnapshot = json;
    this.emit('change', snap);
  }
}

module.exports = { DeviceManager, HEADSET_COLORS, parseAddress };
