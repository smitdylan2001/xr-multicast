const { EventEmitter } = require('events');
const fs = require('fs');
const { StreamSession, listEncoders } = require('./stream-session');
const { QUALITY_PRESETS, CODECS, AUTO_CODEC_ORDER } = require('./config');

const MAX_RETRIES = 5;

class StreamManager extends EventEmitter {
  constructor(adb, devices, store, serverPath) {
    super();
    this.adb = adb;
    this.devices = devices;
    this.store = store;
    this.serverPath = serverPath;
    /** serial -> entry */
    this.entries = new Map();
    /** What the desktop can decode, reported by the renderer: { h264: {any, hw}, ... } */
    this.decoderSupport = { h264: { any: true, hw: false } };
    /** serial -> encoder list (refreshed once per app run) */
    this.encoderCache = new Map();
    /** serial -> Set of codecs that failed this run (skipped until restart) */
    this.codecFailures = new Map();
    /** serial -> Set of codecs whose 10-bit mode failed this run */
    this.tenBitFailures = new Map();
  }

  serverAvailable() {
    return !!this.serverPath && fs.existsSync(this.serverPath);
  }

  setDecoderSupport(support) {
    if (support && typeof support === 'object') this.localDecoderSupport = { h264: { any: true, hw: false }, ...support };
    this.recomputeDecoderSupport();
  }

  /**
   * Remote viewers (tablets/phones) report what they can decode; streams must
   * use a codec every connected viewer can play. Pass null when a viewer leaves.
   * @param {string} clientId
   * @param {Record<string, {any: boolean, hw: boolean}> | null} support
   */
  setViewerSupport(clientId, support) {
    if (!this.viewerSupport) this.viewerSupport = new Map();
    if (support) this.viewerSupport.set(clientId, support);
    else this.viewerSupport.delete(clientId);
    const before = JSON.stringify(this.decoderSupport);
    this.recomputeDecoderSupport();
    if (JSON.stringify(this.decoderSupport) !== before) this.restartIncompatibleStreams();
  }

  recomputeDecoderSupport() {
    const local = this.localDecoderSupport || { h264: { any: true, hw: false } };
    const merged = {};
    for (const [codec, d] of Object.entries(local)) {
      let any = d.any;
      let hw = d.hw;
      for (const viewer of (this.viewerSupport || new Map()).values()) {
        const v = viewer[codec];
        any = any && !!(v && v.any);
        hw = hw && !!(v && v.hw);
      }
      merged[codec] = { any, hw };
    }
    // H.264 is the universal baseline; never let it be "unsupported".
    merged.h264 = { any: true, hw: !!(merged.h264 && merged.h264.hw) };
    this.decoderSupport = merged;
  }

  /** Restart live streams whose codec some viewer can no longer decode. */
  restartIncompatibleStreams() {
    for (const e of this.entries.values()) {
      if (!e.codec || e.userStopped || !['starting', 'live', 'reconnecting'].includes(e.state)) continue;
      const d = this.decoderSupport[e.codec];
      if (!d || !d.any) {
        this.notice('info', `Switching a stream to a codec every viewer can play.`);
        this.start(e.serial, { restart: true, transport: e.preference }).catch(() => {});
      }
    }
  }

  findHeadset(serial) {
    return this.devices.snapshot().headsets.find((h) => h.id === serial) || null;
  }

  notice(level, message) {
    this.emit('notice', { level, message });
  }

  // ----------------------------------------------------------- encoders

  async getEncoders(serial, adbSerial, { refresh = false } = {}) {
    if (!refresh && this.encoderCache.has(serial)) return this.encoderCache.get(serial);
    const list = await listEncoders(this.adb, adbSerial, this.serverPath);
    this.encoderCache.set(serial, list);
    if (this.store.getHeadset(serial)) this.store.upsertHeadset(serial, { encoders: list });
    return list;
  }

  /** For the UI: encoders of a headset (live query if connected, else last known). */
  async encodersFor(serial) {
    const t = this.devices.pickTransport(serial);
    if (t && this.serverAvailable()) return { live: true, encoders: await this.getEncoders(serial, t.adbSerial) };
    const rec = this.store.getHeadset(serial);
    return { live: false, encoders: (rec && rec.encoders) || this.encoderCache.get(serial) || null };
  }

  /**
   * Pick the codec for a stream from the user preference, the headset's
   * encoders and the desktop's decoders.
   */
  async resolveCodec(entry, t) {
    const settings = this.store.settings;
    const pref = entry.codecOverride || settings[t.kind === 'usb' ? 'usbCodec' : 'wifiCodec'] || 'auto';
    const failed = this.codecFailures.get(entry.serial) || new Set();
    const label = (c) => (CODECS[c] ? CODECS[c].label : c);

    let encoders = null;
    try {
      encoders = await this.getEncoders(entry.serial, t.adbSerial);
    } catch (e) {
      // Can't enumerate (very old firmware?) — only H.264 is a safe bet.
      encoders = null;
    }
    const hwEncoder = (c) => (encoders || []).find((e) => e.codec === c && e.hw && !e.alias) || (encoders || []).find((e) => e.codec === c && e.hw);
    const anyEncoder = (c) => (encoders || []).find((e) => e.codec === c);
    const canDecode = (c, needHw) => {
      const d = this.decoderSupport[c];
      return !!d && (needHw ? d.hw : d.any);
    };

    if (pref === 'auto') {
      for (const c of AUTO_CODEC_ORDER) {
        if (c === 'h264') break;
        if (!failed.has(c) && hwEncoder(c) && canDecode(c, true)) return { codec: c, encoder: hwEncoder(c).name };
      }
      return { codec: 'h264', encoder: hwEncoder('h264') ? hwEncoder('h264').name : undefined };
    }
    if (pref !== 'h264') {
      if (failed.has(pref)) return { codec: 'h264', note: `${label(pref)} failed earlier on this headset — using H.264.` };
      if (encoders && !anyEncoder(pref)) return { codec: 'h264', note: `This headset has no ${label(pref)} encoder — using H.264.` };
      if (!canDecode(pref, false)) return { codec: 'h264', note: `This computer can't decode ${label(pref)} — using H.264.` };
      const enc = hwEncoder(pref) || anyEncoder(pref);
      const note = enc && !enc.hw ? `${label(pref)} on this headset is a software encoder and may be slow.` : null;
      return { codec: pref, encoder: enc ? enc.name : undefined, note };
    }
    return { codec: 'h264', encoder: hwEncoder('h264') ? hwEncoder('h264').name : undefined };
  }

  /**
   * Whether to ask for 10-bit (H.265 Main 10 / AV1 Main 10-bit). Only when the
   * experimental setting is on, the codec supports it, every viewer can decode
   * it, and it hasn't already failed for this headset this run.
   */
  useTenBit(entry, codec) {
    if (!this.store.settings.tenBit || entry.forceEightBit) return false;
    if (codec !== 'h265' && codec !== 'av1') return false;
    if ((this.tenBitFailures.get(entry.serial) || new Set()).has(codec)) return false;
    const d = this.decoderSupport[`${codec}_10`];
    return !!(d && d.any);
  }

  markTenBitFailed(serial, codec) {
    if (!this.tenBitFailures.has(serial)) this.tenBitFailures.set(serial, new Set());
    this.tenBitFailures.get(serial).add(codec);
  }

  markCodecFailed(serial, codec) {
    if (!codec || codec === 'h264') return;
    if (!this.codecFailures.has(serial)) this.codecFailures.set(serial, new Set());
    this.codecFailures.get(serial).add(codec);
  }

  qualityFor(kind) {
    const settings = this.store.settings;
    const key = settings[kind === 'usb' ? 'usbQuality' : 'wifiQuality'];
    if (key === 'custom') {
      const c = settings[kind === 'usb' ? 'usbCustom' : 'wifiCustom'] || {};
      const mbps = Math.min(200, Math.max(0.5, Number(c.bitRateMbps) || 8));
      return {
        label: `Custom (${c.maxSize ? `${c.maxSize}px` : 'full res'} · ${mbps} Mbps · ${c.maxFps || 60} fps)`,
        maxSize: Number(c.maxSize) || 0,
        bitRate: Math.round(mbps * 1_000_000),
        maxFps: Number(c.maxFps) || 60,
      };
    }
    return QUALITY_PRESETS[key] || QUALITY_PRESETS.balanced;
  }

  // ------------------------------------------------------------ streams

  /**
   * @param {string} serial
   * @param {{transport?: 'usb'|'wifi'|'auto', restart?: boolean, codec?: string, decodeFailed?: boolean}} opts
   */
  async start(serial, { transport, restart, codec, decodeFailed } = {}) {
    if (!this.serverAvailable()) {
      throw new Error('scrcpy-server is missing. Run "npm run setup" (or reinstall the app).');
    }
    const hs = this.findHeadset(serial);
    if (!hs || hs.status !== 'online') throw new Error('Headset is not connected.');
    if (!hs.canCast) throw new Error(`Verify "${hs.name}" over USB before casting (Settings → require USB verification).`);
    const existing = this.entries.get(serial);
    if (decodeFailed && existing) {
      const label = CODECS[existing.codec] ? CODECS[existing.codec].label : existing.codec;
      if (existing.tenBit) {
        this.markTenBitFailed(serial, existing.codec);
        this.notice('warn', `${hs.name}: 10-bit ${label} could not be decoded — switching to 8-bit.`);
        codec = codec || existing.codec;
      } else if (existing.codec && existing.codec !== 'h264') {
        this.markCodecFailed(serial, existing.codec);
        this.notice('warn', `${hs.name}: this computer could not decode ${label} — switching to H.264.`);
        codec = 'h264';
      }
    }
    if (existing && !existing.userStopped && ['starting', 'live', 'reconnecting'].includes(existing.state)) {
      if (!restart && (!transport || transport === existing.transport)) return;
      await this.stop(serial, { replaced: true });
    }
    const entry = {
      serial,
      preference: transport || hs.transport || 'auto',
      codecOverride: codec || null,
      retries: 0,
      userStopped: false,
      state: 'idle',
    };
    this.entries.set(serial, entry);
    this.launch(entry).catch((e) => this.finish(entry, e.message));
  }

  async eyeCrop(adbSerial, eye) {
    const out = await this.adb.shell(adbSerial, 'wm size', { timeout: 5000 });
    const m = out.match(/Override size:\s*(\d+)x(\d+)/) || out.match(/Physical size:\s*(\d+)x(\d+)/);
    if (!m) return null;
    const w = Number(m[1]);
    const h = Number(m[2]);
    if (w < h * 1.6) return null; // not a side-by-side stereo panel
    const half = Math.floor(w / 2);
    return `${half}:${h}:${eye === 'right' ? half : 0}:0`;
  }

  async launch(entry) {
    const { serial } = entry;
    const t = this.devices.pickTransport(serial, entry.preference) || this.devices.pickTransport(serial, 'auto');
    if (!t) {
      this.finish(entry, 'Headset is no longer connected.');
      return;
    }
    const settings = this.store.settings;
    const preset = this.qualityFor(t.kind);
    const rec = this.store.getHeadset(serial) || {};
    let crop = null;
    if (settings.singleEyeStream) {
      const eye = (rec.calibration && rec.calibration.eye) || 'left';
      crop = await this.eyeCrop(t.adbSerial, eye).catch(() => null);
    }
    const choice = await this.resolveCodec(entry, t);
    if (choice.note && !entry.noted) {
      entry.noted = true;
      this.notice('warn', `${rec.name || serial}: ${choice.note}`);
    }
    if (entry.userStopped || this.entries.get(serial) !== entry) return;

    const tenBit = this.useTenBit(entry, choice.codec);
    const session = new StreamSession(this.adb, t.adbSerial, this.serverPath, {
      ...preset,
      crop,
      codec: choice.codec,
      encoder: choice.encoder,
      // MediaCodecInfo.CodecProfileLevel: HEVCProfileMain10 = 2, AV1ProfileMain10 = 2
      codecOptions: tenBit ? ['profile=2'] : [],
    });
    entry.tenBit = tenBit;
    entry.session = session;
    entry.transport = t.kind;
    entry.adbSerial = t.adbSerial;
    entry.quality = preset.label;
    entry.bitRate = preset.bitRate;
    entry.codec = choice.codec;
    entry.encoder = choice.encoder || null;
    entry.crop = crop;
    entry.error = null;
    entry.gotMedia = false;
    this.setState(entry, entry.retries ? 'reconnecting' : 'starting');

    session.on('device', (name) => (entry.deviceName = name));
    session.on('session', (s) => {
      entry.width = s.width;
      entry.height = s.height;
      this.setState(entry, 'live');
    });
    session.on('packet', (p) => {
      if (!entry.gotMedia && !p.config) {
        entry.gotMedia = true;
        entry.retries = 0;
      }
      this.emit('packet', serial, p);
    });
    session.on('log', (line) => this.emit('log', serial, line));
    session.on('error', (e) => (entry.error = e.message));
    session.on('end', () => {
      if (entry.session !== session) return;
      entry.session = null;
      if (entry.userStopped) {
        if (this.entries.get(serial) === entry) this.entries.delete(serial);
        if (!entry.replaced) this.setState(entry, 'stopped');
        return;
      }
      const hs = this.findHeadset(serial);
      const online = hs && hs.status === 'online';
      // 10-bit refused by the encoder: retry the same codec in 8-bit first.
      if (online && entry.tenBit && !entry.gotMedia) {
        this.markTenBitFailed(serial, entry.codec);
        this.notice('warn', `${(hs && hs.name) || serial}: 10-bit ${CODECS[entry.codec].label} did not start (${entry.error || 'encoder error'}) — using 8-bit.`);
        entry.forceEightBit = true;
        entry.codecOverride = entry.codec;
        setTimeout(() => {
          if (!entry.userStopped && this.entries.get(serial) === entry) this.launch(entry).catch((e) => this.finish(entry, e.message));
        }, 300);
        return;
      }
      // A non-H.264 stream that dies before producing any video almost always
      // means the encoder rejected the configuration: fall back to H.264 now.
      if (online && entry.codec !== 'h264' && !entry.gotMedia) {
        this.markCodecFailed(serial, entry.codec);
        this.notice('warn', `${(hs && hs.name) || serial}: ${CODECS[entry.codec].label} did not start (${entry.error || 'encoder error'}) — falling back to H.264.`);
        entry.codecOverride = 'h264';
        setTimeout(() => {
          if (!entry.userStopped && this.entries.get(serial) === entry) this.launch(entry).catch((e) => this.finish(entry, e.message));
        }, 300);
        return;
      }
      const canRetry = settings.autoRetryStreams && online && entry.retries < MAX_RETRIES;
      if (canRetry) {
        entry.retries++;
        this.setState(entry, 'reconnecting');
        setTimeout(() => {
          if (!entry.userStopped && this.entries.get(serial) === entry) this.launch(entry).catch((e) => this.finish(entry, e.message));
        }, 1500 * entry.retries);
      } else {
        this.finish(entry, entry.error || 'Stream ended.');
      }
    });

    try {
      await session.start();
    } catch (e) {
      entry.error = entry.error || e.message;
      session.stop();
    }
  }

  finish(entry, error) {
    entry.error = error;
    if (this.entries.get(entry.serial) === entry) this.entries.delete(entry.serial);
    this.setState(entry, 'error');
  }

  stateOf(entry) {
    return {
      id: entry.serial,
      state: entry.state,
      transport: entry.transport || null,
      adbSerial: entry.adbSerial || null,
      width: entry.width || null,
      height: entry.height || null,
      deviceName: entry.deviceName || null,
      quality: entry.quality || null,
      bitRate: entry.bitRate || null,
      codec: entry.codec || null,
      tenBit: !!entry.tenBit,
      encoder: entry.encoder || null,
      singleEye: !!entry.crop,
      retries: entry.retries,
      error: entry.state === 'error' || entry.state === 'reconnecting' ? entry.error : null,
    };
  }

  setState(entry, state) {
    entry.state = state;
    this.emit('state', this.stateOf(entry));
  }

  async stop(serial, { replaced = false } = {}) {
    const entry = this.entries.get(serial);
    if (!entry) return;
    entry.userStopped = true;
    entry.replaced = replaced;
    if (entry.session) await entry.session.stop();
    else {
      this.entries.delete(serial);
      if (!replaced) this.setState(entry, 'stopped');
    }
  }

  async startAll() {
    const results = [];
    for (const h of this.devices.snapshot().headsets) {
      if (h.canCast && !this.entries.has(h.id)) {
        results.push(this.start(h.id).then(() => ({ id: h.id, ok: true }), (e) => ({ id: h.id, ok: false, error: e.message })));
      }
    }
    return Promise.all(results);
  }

  async stopAll() {
    await Promise.all([...this.entries.keys()].map((s) => this.stop(s)));
  }

  states() {
    return [...this.entries.values()].map((e) => this.stateOf(e));
  }
}

module.exports = { StreamManager };
