'use strict';

// H.264 Annex B -> fragmented MP4 -> Media Source Extensions.
//
// WebCodecs' VideoDecoder needs a secure context (https or localhost), so a
// tablet browsing http://<pc-ip>:<port> on the LAN does not have it. This
// decoder remuxes the scrcpy H.264 stream into fMP4 fragments (one per frame)
// and plays them in a hidden <video>, with the same interface as
// StreamDecoder. Frames are handed to the lens renderer as
// { source: <video>, displayWidth, displayHeight, close() {} }.
//
// The timeline is synthetic and contiguous (each frame lasts until the next
// one is expected), because scrcpy only sends frames when the screen changes:
// real pts gaps would leave holes in the buffer that stall playback.

(function (root, factory) {
  const api = factory();
  if (typeof module === 'object' && module.exports) module.exports = api;
  else {
    root.Fmp4 = api.Fmp4;
    root.MseDecoder = api.MseDecoder;
  }
})(typeof self !== 'undefined' ? self : this, function () {
  const TIMESCALE = 90000;

  // ------------------------------------------------------------ box writer

  function concat(parts) {
    let len = 0;
    for (const p of parts) len += p.length;
    const out = new Uint8Array(len);
    let o = 0;
    for (const p of parts) {
      out.set(p, o);
      o += p.length;
    }
    return out;
  }

  function u8(...bytes) {
    return Uint8Array.from(bytes);
  }

  function u16(n) {
    return u8((n >>> 8) & 255, n & 255);
  }

  function u32(n) {
    return u8((n >>> 24) & 255, (n >>> 16) & 255, (n >>> 8) & 255, n & 255);
  }

  function u64(n) {
    const hi = Math.floor(n / 0x100000000);
    return concat([u32(hi), u32(n - hi * 0x100000000)]);
  }

  function zeros(n) {
    return new Uint8Array(n);
  }

  function ascii(s) {
    return Uint8Array.from(s, (c) => c.charCodeAt(0));
  }

  function box(type, ...payload) {
    const body = concat(payload);
    return concat([u32(body.length + 8), ascii(type), body]);
  }

  function fullBox(type, version, flags, ...payload) {
    return box(type, u8(version, (flags >>> 16) & 255, (flags >>> 8) & 255, flags & 255), ...payload);
  }

  const MATRIX = concat([u32(0x10000), u32(0), u32(0), u32(0), u32(0x10000), u32(0), u32(0), u32(0), u32(0x40000000)]);

  // ---------------------------------------------------------- H.264 parsing

  /** Split an Annex B buffer into NAL unit payloads (start codes removed). */
  function splitNals(data) {
    const nals = [];
    let start = -1;
    let i = 0;
    const n = data.length;
    while (i + 2 < n) {
      if (data[i] === 0 && data[i + 1] === 0 && data[i + 2] === 1) {
        if (start >= 0) {
          let end = i;
          while (end > start && data[end - 1] === 0) end--;
          nals.push(data.subarray(start, end));
        }
        i += 3;
        start = i;
      } else i++;
    }
    if (start >= 0 && start < n) nals.push(data.subarray(start, n));
    return nals;
  }

  function unescapeRbsp(nal) {
    const out = [];
    let z = 0;
    for (let i = 0; i < nal.length; i++) {
      const b = nal[i];
      if (z >= 2 && b === 3) {
        z = 0;
        continue;
      }
      z = b === 0 ? z + 1 : 0;
      out.push(b);
    }
    return Uint8Array.from(out);
  }

  class BitReader {
    constructor(bytes) {
      this.b = bytes;
      this.pos = 0;
    }
    bit() {
      const byte = this.b[this.pos >> 3];
      if (byte === undefined) throw new Error('SPS truncated');
      const v = (byte >> (7 - (this.pos & 7))) & 1;
      this.pos++;
      return v;
    }
    bits(n) {
      let v = 0;
      for (let i = 0; i < n; i++) v = v * 2 + this.bit();
      return v;
    }
    ue() {
      let lz = 0;
      while (this.bit() === 0) {
        if (++lz > 31) throw new Error('bad exp-golomb');
      }
      return (2 ** lz - 1) + this.bits(lz);
    }
    se() {
      const k = this.ue();
      return k & 1 ? (k + 1) / 2 : -k / 2;
    }
  }

  /** Width/height/profile from an H.264 SPS NAL (with header byte). */
  function parseSps(nal) {
    const r = new BitReader(unescapeRbsp(nal));
    r.bits(8);
    const profile = r.bits(8);
    const compat = r.bits(8);
    const level = r.bits(8);
    r.ue();
    let chroma = 1;
    if ([100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].includes(profile)) {
      chroma = r.ue();
      if (chroma === 3) r.bit();
      r.ue();
      r.ue();
      r.bit();
      if (r.bit()) {
        for (let i = 0; i < (chroma !== 3 ? 8 : 12); i++) {
          if (!r.bit()) continue;
          const size = i < 6 ? 16 : 64;
          let last = 8;
          let next = 8;
          for (let j = 0; j < size; j++) {
            if (next !== 0) next = (last + r.se() + 256) % 256;
            last = next === 0 ? last : next;
          }
        }
      }
    }
    r.ue();
    const pocType = r.ue();
    if (pocType === 0) r.ue();
    else if (pocType === 1) {
      r.bit();
      r.se();
      r.se();
      const n = r.ue();
      for (let i = 0; i < n; i++) r.se();
    }
    r.ue();
    r.bit();
    const wMbs = r.ue() + 1;
    const hMapUnits = r.ue() + 1;
    const frameMbsOnly = r.bit();
    if (!frameMbsOnly) r.bit();
    r.bit();
    let cl = 0;
    let cr = 0;
    let ct = 0;
    let cb = 0;
    if (r.bit()) {
      cl = r.ue();
      cr = r.ue();
      ct = r.ue();
      cb = r.ue();
    }
    const subW = chroma === 1 || chroma === 2 ? 2 : 1;
    const subH = chroma === 1 ? 2 : 1;
    const cropX = chroma === 0 ? 1 : subW;
    const cropY = (chroma === 0 ? 1 : subH) * (2 - frameMbsOnly);
    return {
      profile,
      compat,
      level,
      width: wMbs * 16 - (cl + cr) * cropX,
      height: (2 - frameMbsOnly) * hMapUnits * 16 - (ct + cb) * cropY,
    };
  }

  const hex2 = (n) => n.toString(16).padStart(2, '0');

  /** SPS/PPS + codec info from an Annex B config packet, or null if there is no SPS/PPS. */
  function parseConfig(data) {
    const nals = splitNals(data);
    const sps = nals.filter((n) => (n[0] & 0x1f) === 7);
    const pps = nals.filter((n) => (n[0] & 0x1f) === 8);
    if (!sps.length || !pps.length) return null;
    const info = parseSps(sps[0]);
    return { ...info, sps, pps, codec: `avc1.${hex2(sps[0][1])}${hex2(sps[0][2])}${hex2(sps[0][3])}` };
  }

  function avcC(cfg) {
    const parts = [u8(1, cfg.sps[0][1], cfg.sps[0][2], cfg.sps[0][3], 0xff, 0xe0 | cfg.sps.length)];
    for (const s of cfg.sps) parts.push(u16(s.length), s);
    parts.push(u8(cfg.pps.length));
    for (const p of cfg.pps) parts.push(u16(p.length), p);
    return box('avcC', ...parts);
  }

  // ---------------------------------------------------------- MP4 segments

  function initSegment(cfg) {
    const { width, height } = cfg;
    const ftyp = box('ftyp', ascii('isom'), u32(0x200), ascii('isom'), ascii('iso6'), ascii('avc1'), ascii('mp41'));
    const mvhd = fullBox('mvhd', 0, 0, u32(0), u32(0), u32(1000), u32(0), u32(0x10000), u16(0x100), zeros(10), MATRIX, zeros(24), u32(2));
    const tkhd = fullBox('tkhd', 0, 3, u32(0), u32(0), u32(1), u32(0), u32(0), zeros(8), u16(0), u16(0), u16(0), u16(0), MATRIX, u32(width << 16), u32(height << 16));
    const mdhd = fullBox('mdhd', 0, 0, u32(0), u32(0), u32(TIMESCALE), u32(0), u16(0x55c4), u16(0));
    const hdlr = fullBox('hdlr', 0, 0, u32(0), ascii('vide'), zeros(12), ascii('VideoHandler\0'));
    const vmhd = fullBox('vmhd', 0, 1, zeros(8));
    const dinf = box('dinf', fullBox('dref', 0, 0, u32(1), fullBox('url ', 0, 1)));
    const avc1 = box(
      'avc1',
      zeros(6),
      u16(1),
      zeros(16),
      u16(width),
      u16(height),
      u32(0x480000),
      u32(0x480000),
      u32(0),
      u16(1),
      zeros(32),
      u16(0x18),
      u16(0xffff),
      avcC(cfg),
    );
    const stbl = box(
      'stbl',
      fullBox('stsd', 0, 0, u32(1), avc1),
      fullBox('stts', 0, 0, u32(0)),
      fullBox('stsc', 0, 0, u32(0)),
      fullBox('stsz', 0, 0, u32(0), u32(0)),
      fullBox('stco', 0, 0, u32(0)),
    );
    const minf = box('minf', vmhd, dinf, stbl);
    const trak = box('trak', tkhd, box('mdia', mdhd, hdlr, minf));
    const mvex = box('mvex', fullBox('trex', 0, 0, u32(1), u32(1), u32(0), u32(0), u32(0)));
    return concat([ftyp, box('moov', mvhd, trak, mvex)]);
  }

  /**
   * Annex B access unit -> AVCC (4-byte length prefixed) sample, dropping
   * parameter sets and access unit delimiters (they live in avcC).
   */
  function toAvccSample(data) {
    const parts = [];
    for (const nal of splitNals(data)) {
      const type = nal[0] & 0x1f;
      if (type === 7 || type === 8 || type === 9 || !nal.length) continue;
      parts.push(u32(nal.length), nal);
    }
    return concat(parts);
  }

  const KEY_FLAGS = 0x02000000;
  const DELTA_FLAGS = 0x01010000;

  /** One moof+mdat fragment holding a single sample. */
  function fragment(seq, baseTime, duration, sample, isKey) {
    const tfhd = fullBox('tfhd', 0, 0x020000, u32(1));
    const tfdt = fullBox('tfdt', 1, 0, u64(baseTime));
    const trunSize = 8 + 4 + 4 + 4 + 12;
    // moof header(8) + mfhd(16) + traf header(8) + tfhd(16) + tfdt(20) + trun + mdat header(8)
    const dataOffset = 8 + 16 + 8 + tfhd.length + tfdt.length + trunSize + 8;
    const trun = fullBox('trun', 0, 0x000701, u32(1), u32(dataOffset), u32(duration), u32(sample.length), u32(isKey ? KEY_FLAGS : DELTA_FLAGS));
    const moof = box('moof', fullBox('mfhd', 0, 0, u32(seq)), box('traf', tfhd, tfdt, trun));
    return concat([moof, u32(sample.length + 8), ascii('mdat'), sample]);
  }

  const Fmp4 = { TIMESCALE, box, fullBox, splitNals, parseSps, parseConfig, avcC, initSegment, toAvccSample, fragment, concat };

  // ------------------------------------------------------------ MseDecoder

  const MIME_PROBE = 'video/mp4; codecs="avc1.640033"';
  const LIVE_EDGE_JUMP = 0.4; // s behind the buffered end before seeking to it
  const CATCH_UP = 0.15; // s behind before playing slightly faster
  const MAX_QUEUE = 90; // segments waiting for the SourceBuffer before dropping to the next key frame

  // Autoplay can be refused (e.g. iOS Low Power Mode): retry playback on the next tap.
  const activeVideos = new Set();
  let unlockBound = false;
  function bindUnlock() {
    if (unlockBound) return;
    unlockBound = true;
    const kick = () => {
      for (const v of activeVideos) if (v.paused) v.play().catch(() => {});
    };
    document.addEventListener('pointerup', kick, true);
    document.addEventListener('touchend', kick, true);
  }

  function mediaSourceClass() {
    if (typeof self === 'undefined') return null;
    return self.ManagedMediaSource || self.MediaSource || self.WebKitMediaSource || null;
  }

  class MseDecoder {
    constructor({ onFrame, onError, family = 'h264' }) {
      this.onFrame = onFrame;
      this.onError = onError || (() => {});
      this.family = family;
      this.errors = 0;
      this.dropped = 0;
      this.closed = false;
      this.cfg = null;
      this.video = null;
      this.ms = null;
      this.sb = null;
      this.queue = [];
      this.waitKey = true;
      this.seq = 1;
      this.time = 0;
      this.lastPts = null;
      this.keyTimes = [];
      this.lastFrameTime = -1;
      this.trimTimer = null;
      if (family !== 'h264') this.unsupported();
    }

    static get supported() {
      const MS = mediaSourceClass();
      return !!MS && typeof MS.isTypeSupported === 'function' && MS.isTypeSupported(MIME_PROBE);
    }

    /** What remote viewers using this path report to the hub. */
    static support() {
      const any = MseDecoder.supported;
      return { h264: { any, hw: true }, h265: { any: false, hw: false }, av1: { any: false, hw: false } };
    }

    unsupported() {
      this.errors++;
      this.onError(new Error(`${this.family} cannot be played in this browser (H.264 only)`), this.errors);
    }

    setFamily(family) {
      if (!family || family === this.family) return;
      this.family = family;
      this.teardown();
      this.cfg = null;
      this.waitKey = true;
      if (family !== 'h264') this.unsupported();
    }

    push(isConfig, isKey, pts, data) {
      if (this.closed || this.family !== 'h264') return;
      if (isConfig) {
        let cfg = null;
        try {
          cfg = parseConfig(data);
        } catch (e) {
          this.fail(e);
          return;
        }
        if (!cfg) return;
        const same = this.cfg && this.cfg.codec === cfg.codec && this.cfg.width === cfg.width && this.cfg.height === cfg.height;
        this.cfg = cfg;
        if (!same || !this.sb) this.setup();
        this.waitKey = true;
        return;
      }
      if (!this.cfg && isKey) {
        // Parameter sets sent in-band with the key frame.
        try {
          this.cfg = parseConfig(data);
        } catch {
          this.cfg = null;
        }
        if (this.cfg) this.setup();
      }
      if (!this.cfg) return;
      if (this.waitKey && !isKey) {
        this.dropped++;
        return;
      }
      if (!isKey && this.queue.length > MAX_QUEUE) {
        this.waitKey = true;
        this.dropped++;
        return;
      }
      const sample = toAvccSample(data);
      if (!sample.length) return;
      let dt = this.lastPts == null ? 1 / 60 : (pts - this.lastPts) / 1e6;
      if (!(dt > 0)) dt = 1 / 60;
      dt = Math.min(1 / 20, Math.max(1 / 240, dt));
      this.lastPts = pts;
      const duration = Math.round(dt * TIMESCALE);
      if (isKey) {
        this.keyTimes.push(this.time / TIMESCALE);
        if (this.keyTimes.length > 64) this.keyTimes.shift();
      }
      this.enqueue(fragment(this.seq++, this.time, duration, sample, isKey));
      this.time += duration;
      this.waitKey = false;
    }

    setup() {
      this.teardown();
      const MS = mediaSourceClass();
      if (!MS) return this.fail(new Error('Media Source Extensions are not available'));
      const v = document.createElement('video');
      v.muted = true;
      v.defaultMuted = true;
      v.playsInline = true;
      v.autoplay = true;
      v.setAttribute('muted', '');
      v.setAttribute('playsinline', '');
      v.setAttribute('webkit-playsinline', '');
      v.disableRemotePlayback = true;
      v.className = 'mse-video';
      // Kept in the layout (not display:none) so browsers keep decoding and presenting frames.
      v.style.cssText = 'position:fixed;left:0;top:0;width:2px;height:2px;opacity:0.01;pointer-events:none;z-index:-1';
      document.body.appendChild(v);
      const ms = new MS();
      this.video = v;
      this.ms = ms;
      this.queue = [this.cfg ? initSegment(this.cfg) : null].filter(Boolean);
      this.time = 0;
      this.seq = 1;
      this.lastPts = null;
      this.keyTimes = [];
      this.lastFrameTime = -1;
      const gen = (this.gen = (this.gen || 0) + 1);
      ms.addEventListener('sourceopen', () => {
        if (gen !== this.gen || this.closed) return;
        try {
          const sb = ms.addSourceBuffer(`video/mp4; codecs="${this.cfg.codec}"`);
          sb.mode = 'segments';
          sb.addEventListener('updateend', () => this.pump());
          sb.addEventListener('error', () => this.fail(new Error('SourceBuffer error')));
          this.sb = sb;
          this.pump();
        } catch (e) {
          this.fail(e);
        }
      });
      v.addEventListener('error', () => {
        if (gen === this.gen) this.fail(new Error(`video error ${v.error ? v.error.code : ''}: ${(v.error && v.error.message) || 'decode failed'}`));
      });
      v.src = URL.createObjectURL(ms);
      activeVideos.add(v);
      bindUnlock();
      this.watchFrames(v, gen);
      this.trimTimer = setInterval(() => this.maintain(), 1000);
    }

    watchFrames(v, gen) {
      const deliver = () => {
        if (gen !== this.gen || this.closed || !v.videoWidth) return;
        this.onFrame({ source: v, displayWidth: v.videoWidth, displayHeight: v.videoHeight, close() {} });
      };
      let lastCallback = 0;
      const hasRvfc = typeof v.requestVideoFrameCallback === 'function';
      if (hasRvfc) {
        const cb = () => {
          if (gen !== this.gen || this.closed) return;
          lastCallback = performance.now();
          deliver();
          v.requestVideoFrameCallback(cb);
        };
        v.requestVideoFrameCallback(cb);
      }
      // Timer path: browsers without requestVideoFrameCallback, and pages that are
      // not being painted (occluded windows), where the callback never fires.
      this.frameTimer = setInterval(
        () => {
          if (hasRvfc && performance.now() - lastCallback < 250) return;
          if (v.readyState >= 2 && v.currentTime !== this.lastFrameTime) {
            this.lastFrameTime = v.currentTime;
            deliver();
          }
        },
        hasRvfc ? 50 : 15,
      );
    }

    enqueue(seg) {
      this.queue.push(seg);
      this.pump();
    }

    pump() {
      const sb = this.sb;
      if (!sb || sb.updating || !this.queue.length || this.closed) return;
      const data = this.queue.length === 1 ? this.queue[0] : concat(this.queue);
      const pending = this.queue;
      this.queue = [];
      try {
        sb.appendBuffer(data);
      } catch (e) {
        if (e && e.name === 'QuotaExceededError') {
          // Free everything before the current GOP and resume at the next key frame.
          this.queue = pending[0] && this.isInit(pending[0]) ? [pending[0]] : [];
          this.waitKey = true;
          this.trim(true);
          return;
        }
        this.fail(e);
        return;
      }
      this.syncPlayback();
    }

    isInit(seg) {
      return seg.length > 8 && seg[4] === 0x66 && seg[5] === 0x74 && seg[6] === 0x79 && seg[7] === 0x70; // 'ftyp'
    }

    syncPlayback() {
      const v = this.video;
      if (!v || !this.sb) return;
      let b;
      try {
        b = this.sb.buffered;
      } catch {
        return;
      }
      if (!b.length) return;
      const end = b.end(b.length - 1);
      const start = b.start(b.length - 1);
      const behind = end - v.currentTime;
      // Also pull back a playhead that ran past the data (seen in hidden/background pages after a stall).
      if (v.currentTime < start || behind > LIVE_EDGE_JUMP || behind < -0.3) v.currentTime = Math.max(start, end - 0.05);
      const rate = behind > CATCH_UP ? 1.15 : 1;
      if (v.playbackRate !== rate) v.playbackRate = rate;
      if (v.paused) v.play().catch(() => {});
    }

    maintain() {
      this.syncPlayback();
      this.trim(false);
    }

    /** Remove buffered media before the newest key frame we no longer need. */
    trim(force) {
      const v = this.video;
      const sb = this.sb;
      if (!v || !sb || sb.updating) return;
      let b;
      try {
        b = sb.buffered;
      } catch {
        return;
      }
      if (!b.length) return;
      const keep = force ? 0.5 : 3;
      const cut = this.keyTimes.filter((t) => t <= v.currentTime - keep).pop();
      if (cut == null || cut <= b.start(0) + 0.01) return;
      if (!force && v.currentTime - b.start(0) < 8) return;
      try {
        sb.remove(0, cut);
      } catch {
        /* busy: next round */
      }
    }

    fail(e) {
      if (this.closed) return;
      this.errors++;
      this.onError(e, this.errors);
      // Rebuild the pipeline and restart at the next key frame.
      const cfg = this.cfg;
      this.teardown();
      this.cfg = cfg;
      this.waitKey = true;
      if (cfg && this.errors < 20) this.setup();
    }

    teardown() {
      this.gen = (this.gen || 0) + 1;
      clearInterval(this.trimTimer);
      clearInterval(this.frameTimer);
      this.trimTimer = null;
      this.frameTimer = null;
      this.queue = [];
      this.sb = null;
      if (this.ms && this.ms.readyState === 'open') {
        try {
          this.ms.endOfStream();
        } catch {
          /* ignore */
        }
      }
      this.ms = null;
      if (this.video) {
        const v = this.video;
        activeVideos.delete(v);
        const src = v.src;
        v.removeAttribute('src');
        v.srcObject = null;
        try {
          v.load();
        } catch {
          /* ignore */
        }
        if (src && src.startsWith('blob:')) URL.revokeObjectURL(src);
        v.remove();
      }
      this.video = null;
    }

    close() {
      this.closed = true;
      this.teardown();
    }
  }

  return { Fmp4, MseDecoder };
});
