'use strict';

/* global xr, LensRenderer, LayoutDetect, StreamDecoder, MseDecoder, icon, hydrateIcons */

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];
const esc = (s) =>
  String(s == null ? '' : s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);

const S = {
  brands: {},
  colors: [],
  presets: {},
  codecs: {},
  decoderSupport: {},
  settings: {},
  env: {},
  headsets: [],
  adbError: null,
  streams: new Map(), // id -> latest stream state from main
  tiles: new Map(), // id -> Tile
  focus: null,
  calib: null, // { id, params, grid, original }
};

const VIEW_MODES = [
  { id: 'corrected', label: 'Corrected', title: 'Single eye, lens distortion removed (recommended)' },
  { id: 'stereo', label: 'Both eyes', title: 'Both eyes side by side, lens distortion removed' },
  { id: 'raw', label: 'Raw', title: 'Unprocessed panel image (fisheye, both eyes)' },
];

const LAYOUT_LABELS = { auto: 'Auto detect', sbs: 'Side by side', tb: 'Top / bottom', quad: '2×2 grid', mono: 'Single image' };

const headsetById = (id) => S.headsets.find((h) => h.id === id) || null;
const brandOf = (h) => S.brands[(h && h.brand) || 'android'] || S.brands.android;
const presetFor = (h) => {
  const b = brandOf(h);
  const v = h && h.variant && (b.variants || []).find((x) => x.id === h.variant);
  return { ...b.correction, ...((v && v.correction) || {}) };
};
const correctionFor = (h) => {
  const cal = { ...((h && h.calibration) || {}) };
  // Calibrations saved before 'auto' rotation existed stored the old default of 0°.
  if (!('flipH' in cal) && Number(cal.rotation) === 0) delete cal.rotation;
  return { ...presetFor(h), ...cal };
};
const labelOf = (h) => (h ? `${h.index ? `#${h.index} ` : ''}${h.name}` : 'Headset');
// Remote browsers (tablets) may be view-only; the hub enforces this too.
const canControl = () => !S.env.remote || !!S.env.canControl;

// ------------------------------------------------------------------ toasts

function toast(message, level = 'info', ms = 5000) {
  const el = document.createElement('div');
  el.className = `toast ${level}`;
  el.innerHTML = `<i data-icon="${level === 'error' || level === 'warn' ? 'alert' : level === 'ok' ? 'check' : 'help'}"></i><span>${esc(message)}</span>`;
  hydrateIcons(el);
  $('#toasts').appendChild(el);
  setTimeout(() => {
    el.classList.add('out');
    setTimeout(() => el.remove(), 300);
  }, ms);
}

async function run(promise, okMessage) {
  try {
    const v = await promise;
    if (okMessage) toast(okMessage, 'ok');
    return v;
  } catch (e) {
    toast(e.message || String(e), 'error', 8000);
    return undefined;
  }
}

// ------------------------------------------------------------------ modals

function openModal({ title, body, actions = [], wide = false, onClose }) {
  const root = $('#modal-root');
  const wrap = document.createElement('div');
  wrap.className = 'modal-backdrop';
  wrap.innerHTML = `
    <div class="modal ${wide ? 'wide' : ''}" role="dialog" aria-modal="true">
      <header><h3>${esc(title)}</h3><button class="btn icon ghost" data-close title="Close"><i data-icon="x"></i></button></header>
      <div class="modal-body"></div>
      <footer class="modal-actions"></footer>
    </div>`;
  const bodyEl = $('.modal-body', wrap);
  if (typeof body === 'string') bodyEl.innerHTML = body;
  else if (body) bodyEl.appendChild(body);
  const close = () => {
    wrap.remove();
    document.removeEventListener('keydown', onKey);
    if (onClose) onClose();
  };
  const onKey = (e) => {
    if (e.key === 'Escape') close();
  };
  document.addEventListener('keydown', onKey);
  const footer = $('.modal-actions', wrap);
  if (!actions.length) footer.remove();
  for (const a of actions) {
    const b = document.createElement('button');
    b.className = `btn ${a.kind || ''}`;
    b.textContent = a.label;
    b.addEventListener('click', async () => {
      if (!a.onClick) return close();
      b.disabled = true;
      const keep = await a.onClick(wrap);
      b.disabled = false;
      if (keep !== true) close();
    });
    footer.appendChild(b);
  }
  $('[data-close]', wrap).addEventListener('click', close);
  wrap.addEventListener('mousedown', (e) => {
    if (e.target === wrap) close();
  });
  hydrateIcons(wrap);
  root.appendChild(wrap);
  const first = $('input, select', wrap);
  if (first) first.focus();
  return { el: wrap, close };
}

// -------------------------------------------------------------------- tile

class Tile {
  constructor(id) {
    this.id = id;
    const h = headsetById(id);
    this.viewMode = (h && h.viewMode) || S.settings.defaultViewMode || 'corrected';
    this.el = document.createElement('section');
    this.el.className = 'tile';
    this.el.dataset.id = id;
    this.el.innerHTML = `
      <header class="tile-head">
        <span class="tile-index"></span>
        <div class="tile-titles">
          <span class="tile-name"></span>
          <span class="tile-sub"></span>
        </div>
        <span class="spacer"></span>
        <span class="brand-badge"></span>
        <span class="conn-badge"></span>
        <span class="live-badge">LIVE</span>
      </header>
      <div class="tile-body">
        <canvas></canvas>
        <div class="video-label"></div>
        <div class="tile-status"></div>
      </div>
      <footer class="tile-foot">
        <div class="seg view-seg">${VIEW_MODES.map((m) => `<button data-view="${m.id}" title="${esc(m.title)}">${m.label}</button>`).join('')}</div>
        <div class="seg eye-seg" title="Which eye to show"><button data-eye="left">L</button><button data-eye="right">R</button></div>
        <span class="tile-stats"></span>
        <span class="spacer"></span>
        <button class="btn icon ghost" data-tile="rotate" title="Rotate 90° clockwise (experimental)"><i data-icon="rotate"></i></button>
        <button class="btn icon ghost" data-tile="calibrate" title="Calibrate lens correction"><i data-icon="sliders"></i></button>
        <button class="btn icon ghost" data-tile="snapshot" title="Save a snapshot (PNG)"><i data-icon="camera"></i></button>
        <button class="btn icon ghost" data-tile="fullscreen" title="Fullscreen"><i data-icon="expand"></i></button>
        <button class="btn icon ghost danger" data-tile="stop" title="Stop casting"><i data-icon="stop"></i></button>
      </footer>`;
    hydrateIcons(this.el);
    this.canvas = $('canvas', this.el);
    this.statusEl = $('.tile-status', this.el);
    this.statsEl = $('.tile-stats', this.el);

    try {
      this.renderer = new LensRenderer(this.canvas);
    } catch (e) {
      this.renderer = null;
      this.fatal = e.message;
    }
    if (!StreamDecoder.supported && !(window.MseDecoder && MseDecoder.supported)) this.fatal = 'This browser cannot decode the video (no WebCodecs or Media Source Extensions).';
    this.decoder = this.makeDecoder();

    this.pendingFrame = null;
    this.hasVideo = false;
    this.dirty = true;
    this.frames = 0;
    this.bytes = 0;
    this.fps = 0;
    this.mbps = 0;
    this.lastStat = performance.now();
    this.drawInfo = null;

    this.el.addEventListener('click', (e) => this.onClick(e));
    $('.tile-body', this.el).addEventListener('dblclick', () => setFocus(S.focus === id ? null : id));
    this.refresh();
  }

  onClick(e) {
    const view = e.target.closest('[data-view]');
    const eye = e.target.closest('[data-eye]');
    const act = e.target.closest('[data-tile]');
    const h = headsetById(this.id);
    if (view) {
      this.viewMode = view.dataset.view;
      if (h && h.trusted) xr.update(this.id, { viewMode: this.viewMode }).catch(() => {});
      this.refresh();
    } else if (eye) {
      if (S.calib && S.calib.id === this.id) {
        S.calib.params.eye = eye.dataset.eye;
        renderCalibration();
      } else if (h && h.trusted) {
        const st = S.streams.get(this.id);
        xr.update(this.id, { calibration: { ...correctionFor(h), eye: eye.dataset.eye } })
          .then(() => {
            // In single-eye streaming the crop happens on the headset, so switching eyes needs a restart.
            if (st && st.singleEye) startStream(this.id, { restart: true });
          })
          .catch((err) => toast(err.message, 'error'));
      }
      this.eyeOverride = eye.dataset.eye;
      this.refresh();
    } else if (act) {
      const a = act.dataset.tile;
      if (a === 'stop') stopStream(this.id);
      else if (a === 'calibrate') openCalibration(this.id);
      else if (a === 'rotate') this.rotate();
      else if (a === 'fullscreen') toggleFullscreen(this.el);
      else if (a === 'snapshot') this.snapshot();
      else if (a === 'retry') startStream(this.id);
      else if (a === 'close') removeTile(this.id);
    }
  }

  params() {
    const h = headsetById(this.id);
    const base = S.calib && S.calib.id === this.id ? S.calib.params : correctionFor(h);
    const p = { ...base, viewMode: this.viewMode, grid: !!(S.calib && S.calib.id === this.id && S.calib.grid) };
    if (this.eyeOverride && !(S.calib && S.calib.id === this.id)) p.eye = this.eyeOverride;
    if (this.rotationOverride != null && !(S.calib && S.calib.id === this.id)) p.rotation = this.rotationOverride;
    const st = S.streams.get(this.id);
    const v = this.resolve(p);
    p.wantsDetection = !(st && st.singleEye) && (p.layout === 'auto' || p.rotation === 'auto');
    p.layout = v.layout;
    p.rotation = v.rotation;
    p.resolved = v;
    return p;
  }

  /** Concrete layout/rotation for correction params whose layout or rotation may be 'auto'. */
  resolve(p) {
    const r = this.renderer;
    const st = S.streams.get(this.id);
    return LayoutDetect.resolveView({
      layout: p.layout,
      rotation: p.rotation,
      autoRotation: p.autoRotation,
      frameW: r ? r.frameW : 0,
      frameH: r ? r.frameH : 0,
      grid: this.grid,
      singleEye: !!(st && st.singleEye),
    });
  }

  /** Rotate the view a further 90° clockwise; turns 'auto' rotation into an explicit angle. */
  rotate() {
    const next = (this.params().rotation + 90) % 360;
    const h = headsetById(this.id);
    if (S.calib && S.calib.id === this.id) {
      S.calib.params.rotation = next;
      renderCalibration();
    } else {
      this.rotationOverride = next;
      if (h && h.trusted) xr.update(this.id, { calibration: { ...correctionFor(h), rotation: next } }).catch((err) => toast(err.message, 'error'));
    }
    this.dirty = true;
    this.updateStats();
  }

  /** Detect the panel's view grid from the video: on the first frames, then every few seconds. */
  maybeDetect(now) {
    if (!this.renderer || !this.renderer.hasFrame || now < (this.nextDetect || 0)) return;
    if (!this.params().wantsDetection) {
      this.nextDetect = now + 3000;
      return;
    }
    if (!this.detectSince) this.detectSince = now;
    let res = null;
    try {
      // Analyse the unrotated frame; resolveView() maps the grid through the rotation.
      res = this.renderer.analyze({ rotation: 0 });
    } catch (e) {
      console.warn('layout detection failed', e);
    }
    const g = res && res.grid;
    if (g) {
      const key = `${g.cols}x${g.rows}`;
      // Only switch once the same grid was seen twice in a row.
      if (key === this.gridCandidate && (!this.grid || `${this.grid.cols}x${this.grid.rows}` !== key)) {
        this.grid = { cols: g.cols, rows: g.rows };
        this.dirty = true;
        this.updateStats();
        if (S.calib && S.calib.id === this.id) syncCalibrationAuto();
      }
      this.gridCandidate = key;
    }
    const age = now - this.detectSince;
    this.nextDetect = now + (!this.grid ? 500 : age < 30000 ? 3000 : 30000);
  }

  makeDecoder(family) {
    const st = S.streams.get(this.id);
    // WebCodecs needs a secure context; LAN browsers (http://<hub-ip>) fall back to MSE.
    const Decoder = window.VideoDecoder && window.isSecureContext ? StreamDecoder : window.MseDecoder || StreamDecoder;
    return new Decoder({
      family: family || (st && st.codec) || 'h264',
      onFrame: (f) => this.onFrame(f),
      onError: (e, count) => {
        console.warn(`[${this.id}] decoder:`, e);
        if (count >= 3) this.codecFallback(`decoder error: ${e.message || e}`);
      },
    });
  }

  /** Ask main to restart this stream in H.264 when the desktop can't decode the current codec. */
  codecFallback(reason) {
    const st = S.streams.get(this.id);
    // 10-bit falls back to 8-bit of the same codec first, then H.264.
    const key = st && `${st.codec}:${st.tenBit ? 10 : 8}`;
    if (!st || !st.codec || (st.codec === 'h264' && !st.tenBit) || this.fallbackRequested === key) return;
    this.fallbackRequested = key;
    console.warn(`[${this.id}] decode fallback (${key}): ${reason}`);
    startStream(this.id, { restart: true, decodeFailed: true });
  }

  push(isConfig, isKey, pts, data) {
    this.bytes += data.byteLength;
    if (!this.firstPacketAt) this.firstPacketAt = performance.now();
    this.decoder.push(isConfig, isKey, pts, data);
  }

  onFrame(frame) {
    if (this.pendingFrame) this.pendingFrame.close();
    this.pendingFrame = frame;
    this.frames++;
    // requestAnimationFrame is throttled when the window is occluded (e.g. while
    // captured by OBS behind other windows); keep rendering with a timer fallback.
    if (!this.fallbackTimer) {
      this.fallbackTimer = setTimeout(() => {
        this.fallbackTimer = null;
        if (this.pendingFrame && !this.el.hidden) this.tick(performance.now());
      }, 50);
    }
  }

  tick(now) {
    if (this.fallbackTimer) {
      clearTimeout(this.fallbackTimer);
      this.fallbackTimer = null;
    }
    const dt = now - this.lastStat;
    if (dt >= 1000) {
      this.fps = (this.frames * 1000) / dt;
      this.mbps = (this.bytes * 8) / dt / 1000;
      this.frames = 0;
      this.bytes = 0;
      this.lastStat = now;
      this.updateStats();
      // Packets arriving but nothing decoded for a while: the codec isn't usable here.
      if (!this.hasVideo && this.firstPacketAt && now - this.firstPacketAt > 6000) this.codecFallback('no frames decoded');
    }
    if (!this.renderer) return;
    if (this.pendingFrame) {
      try {
        this.renderer.upload(this.pendingFrame);
      } catch (e) {
        console.warn('upload failed', e);
      }
      this.pendingFrame.close();
      this.pendingFrame = null;
      this.dirty = true;
      if (!this.hasVideo) {
        this.hasVideo = true;
        this.refresh();
      }
      this.maybeDetect(now);
    }
    if (this.dirty || this.renderer.needsResize()) {
      const p = this.params();
      this.view = p.resolved;
      this.drawInfo = this.renderer.draw(p);
      this.dirty = false;
    }
  }

  updateStats() {
    const st = S.streams.get(this.id);
    if (!st || st.state !== 'live' || !this.hasVideo) {
      this.statsEl.textContent = '';
      return;
    }
    const r = this.renderer;
    const parts = [];
    const depth = this.decoder && this.decoder.bitDepth > 8 ? ` ${this.decoder.bitDepth}-bit` : '';
    if (st.codec) parts.push((S.codecs[st.codec] ? S.codecs[st.codec].label.replace(/ \(.*\)/, '') : st.codec) + depth);
    if (r && r.frameW) parts.push(`${r.frameW}×${r.frameH}`);
    const v = r && r.frameW ? this.params().resolved : null;
    if (v) {
      const grid = { sbs: '2×1', tb: '1×2', quad: '2×2', mono: st.singleEye ? 'one eye' : '1×1' }[v.layout];
      parts.push(`${grid}${v.detected ? ' auto' : ''}${v.rotation ? ` ↻${v.rotation}°` : ''}`);
    }
    parts.push(`${this.fps.toFixed(0)} fps`);
    parts.push(`${this.mbps.toFixed(1)} Mbps`);
    this.statsEl.textContent = parts.join(' · ');
    const layoutNote = v && `Panel layout: ${LAYOUT_LABELS[v.layout] || v.layout}${v.detected ? ' (detected)' : ''}, rotation ${v.rotation}°${v.rotationAuto ? ' (auto)' : ''}`;
    this.statsEl.title = [st.quality, st.encoder && `Encoder: ${st.encoder}`, layoutNote].filter(Boolean).join('\n');
  }

  snapshot() {
    if (!this.renderer || !this.renderer.hasFrame) return toast('No video yet.', 'warn');
    this.renderer.draw({ ...this.params(), grid: false });
    const url = this.canvas.toDataURL('image/png');
    this.dirty = true;
    const h = headsetById(this.id);
    const a = document.createElement('a');
    const stamp = new Date().toISOString().replace(/[:.]/g, '-').slice(0, 19);
    a.href = url;
    a.download = `${(h ? h.name : this.id).replace(/[^\w\- ]+/g, '_')}-${stamp}.png`;
    a.click();
  }

  refresh() {
    const h = headsetById(this.id);
    const st = S.streams.get(this.id) || { state: 'starting' };
    const b = brandOf(h);
    const color = (h && h.color) || b.color;
    this.el.style.setProperty('--hs-color', color);
    this.el.style.setProperty('--brand-color', b.color);
    $('.tile-index', this.el).textContent = h && h.index ? `#${h.index}` : '•';
    $('.tile-name', this.el).textContent = h ? h.name : this.id;
    $('.tile-sub', this.el).textContent = h ? `${b.family} · ${h.model}${h.battery ? ` · ${h.battery.level}%${h.battery.charging ? ' ⚡' : ''}` : ''}` : '';
    $('.brand-badge', this.el).textContent = b.name;
    const conn = $('.conn-badge', this.el);
    if (st.transport) {
      conn.innerHTML = `${icon(st.transport === 'usb' ? 'usb' : 'wifi')}<span>${st.transport === 'usb' ? 'USB' : `Wi-Fi${h && h.ip ? ` ${esc(h.ip)}` : ''}`}</span>`;
      conn.hidden = false;
    } else conn.hidden = true;
    $('.video-label', this.el).innerHTML = `<b>${esc(labelOf(h))}</b><span>${esc(b.name)} ${esc(h ? h.model : '')}</span>`;

    this.el.classList.toggle('live', st.state === 'live' && this.hasVideo);
    this.el.classList.toggle('errored', st.state === 'error');
    for (const btn of $$('[data-view]', this.el)) btn.classList.toggle('active', btn.dataset.view === this.viewMode);
    const eye = this.params().eye;
    for (const btn of $$('[data-eye]', this.el)) btn.classList.toggle('active', btn.dataset.eye === eye);
    $('.eye-seg', this.el).hidden = this.viewMode !== 'corrected';

    let status = '';
    let actions = '';
    if (this.fatal) status = `<b>Cannot display video</b><span>${esc(this.fatal)}</span>`;
    else if (st.state === 'starting') status = `<div class="spinner"></div><b>Starting stream…</b><span>${st.transport ? `via ${st.transport === 'usb' ? 'USB' : 'Wi-Fi'}` : ''}</span>`;
    else if (st.state === 'reconnecting')
      status = `<div class="spinner"></div><b>Reconnecting… (attempt ${st.retries || 1})</b><span>${esc(st.error || '')}</span>`;
    else if (st.state === 'live' && !this.hasVideo) status = `<div class="spinner"></div><b>Waiting for video…</b><span>Wake the headset if its display is off.</span>`;
    else if (st.state === 'error') {
      status = `<b>Stream stopped</b><span>${esc(st.error || 'Unknown error')}</span>`;
      actions = `<div class="row"><button class="btn primary" data-tile="retry">Retry</button><button class="btn" data-tile="close">Close</button></div>`;
    }
    this.statusEl.innerHTML = status + actions;
    this.statusEl.hidden = !status;
    this.dirty = true;
    this.updateStats();
  }

  resetVideo(family) {
    this.decoder.close();
    this.decoder = this.makeDecoder(family);
    this.hasVideo = false;
    this.firstPacketAt = 0;
    // Keep the last detected grid (avoids flicker on reconnect) but re-check soon.
    this.nextDetect = 0;
    this.detectSince = 0;
    this.gridCandidate = null;
  }

  destroy() {
    clearTimeout(this.fallbackTimer);
    this.decoder.close();
    if (this.pendingFrame) this.pendingFrame.close();
    if (this.renderer) this.renderer.dispose();
    this.el.remove();
  }
}

function ensureTile(id) {
  let t = S.tiles.get(id);
  if (!t) {
    t = new Tile(id);
    S.tiles.set(id, t);
    $('#grid').appendChild(t.el);
    layoutGrid();
  }
  return t;
}

function removeTile(id) {
  const t = S.tiles.get(id);
  if (!t) return;
  if (S.calib && S.calib.id === id) closeCalibration(false);
  t.destroy();
  S.tiles.delete(id);
  S.streams.delete(id);
  if (S.focus === id) S.focus = null;
  layoutGrid();
  renderSidebar();
}

function layoutGrid() {
  const grid = $('#grid');
  const n = S.tiles.size;
  grid.hidden = n === 0;
  const cols = S.settings.gridColumns || 'auto';
  let c;
  if (S.focus && S.tiles.has(S.focus)) c = 1;
  else if (cols !== 'auto') c = Number(cols);
  else c = n <= 1 ? 1 : n <= 4 ? 2 : n <= 9 ? 3 : 4;
  grid.style.setProperty('--cols', c);
  const rows = Math.max(1, Math.ceil((S.focus ? 1 : n) / c));
  grid.style.setProperty('--rows', rows);
  for (const [id, t] of S.tiles) {
    t.el.hidden = !!(S.focus && S.focus !== id);
    t.el.classList.toggle('focused', S.focus === id);
  }
  // Remote browsers only receive video for the tiles they show.
  if (xr.setVisibleStreams) xr.setVisibleStreams([...S.tiles].filter(([, t]) => !t.el.hidden).map(([id]) => id));
  updateEmptyState();
}

function updateEmptyState() {
  const anyKnown = S.headsets.length > 0;
  const empty = $('#empty');
  empty.hidden = S.tiles.size > 0;
  empty.classList.toggle('has-headsets', anyKnown);
  const h1 = $('h1', empty);
  if (anyKnown) {
    const castable = S.headsets.filter((h) => h.canCast).length;
    h1.textContent = castable ? `${castable} headset${castable > 1 ? 's' : ''} ready — click Cast or Cast all` : 'Verify a headset to start casting';
  } else {
    h1.textContent = 'Connect your first headset';
  }
}

/** Fullscreen with the WebKit prefix (iPad Safari), or a CSS fallback where element fullscreen is missing (iPhone). */
function toggleFullscreen(el) {
  const d = document;
  if (d.fullscreenElement || d.webkitFullscreenElement) return (d.exitFullscreen || d.webkitExitFullscreen).call(d);
  if (el.classList.contains('pseudo-full')) return el.classList.remove('pseudo-full');
  const req = el.requestFullscreen || el.webkitRequestFullscreen;
  const fallback = () => el.classList.add('pseudo-full');
  if (!req) return fallback();
  try {
    const p = req.call(el);
    if (p && p.catch) p.catch(fallback);
  } catch {
    fallback();
  }
}

function setFocus(id) {
  S.focus = id && S.tiles.has(id) ? id : null;
  layoutGrid();
}

// ---------------------------------------------------------------- streams

async function startStream(id, opts) {
  const h = headsetById(id);
  if (!h) return;
  const t = ensureTile(id);
  t.resetVideo();
  S.streams.set(id, { id, state: 'starting' });
  t.refresh();
  try {
    await xr.startStream(id, opts);
  } catch (e) {
    S.streams.set(id, { id, state: 'error', error: e.message });
    t.refresh();
  }
  renderSidebar();
}

async function stopStream(id) {
  await run(xr.stopStream(id));
  removeTile(id);
}

function onStreamState(st) {
  const prev = S.streams.get(st.id);
  if (st.state === 'stopped') {
    if (S.tiles.has(st.id)) removeTile(st.id);
    S.streams.delete(st.id);
    renderSidebar();
    return;
  }
  S.streams.set(st.id, st);
  const t = ensureTile(st.id);
  if (st.state === 'starting' || st.state === 'reconnecting') {
    if ((prev && prev.state === 'live') || (st.codec && t.decoder.family !== st.codec)) t.resetVideo(st.codec);
  }
  t.refresh();
  renderSidebar();
}

// ---------------------------------------------------------------- sidebar

function batteryHtml(b) {
  if (!b) return '';
  const cls = b.level <= 15 ? 'low' : b.level <= 35 ? 'mid' : '';
  return `<span class="battery ${cls}" title="Battery ${b.level}%${b.charging ? ' (charging)' : ''}">${icon(b.charging ? 'bolt' : 'battery')}${b.level}%</span>`;
}

function headsetCard(h) {
  const b = brandOf(h);
  const st = S.streams.get(h.id);
  const streaming = st && ['starting', 'live', 'reconnecting'].includes(st.state);

  if (h.pending) {
    let body = '';
    if (h.status === 'unauthorized') {
      body = `<div class="hint warn">${icon('alert')}<span>Put the headset on and accept <b>Allow USB debugging</b> (tick "Always allow").</span></div>`;
    } else if (h.status === 'identifying') {
      body = `<div class="hint"><div class="spinner small"></div><span>Identifying…</span></div>`;
    } else if (h.status === 'error') {
      body = `<div class="hint warn">${icon('alert')}<span>${esc(h.error || 'Could not read device info.')}</span></div>`;
    } else {
      body = `<div class="hint warn">${icon('alert')}<span>Device is <b>${esc(h.status)}</b>. Try re-plugging the cable or toggling USB debugging.</span></div>`;
    }
    return `
      <div class="hs-card pending" style="--hs-color:${b.color}">
        <div class="hs-top">
          <span class="hs-dot"></span>
          <div class="hs-title"><b>${esc(h.name || 'New device')}</b><small>${esc(b.family)} · ${h.network ? 'Wi-Fi' : 'USB'}</small></div>
        </div>
        ${body}
      </div>`;
  }

  const chips = [];
  chips.push(`<span class="chip ${h.usb ? 'on' : ''}" title="${h.usb ? 'Connected over USB' : 'Not connected over USB'}">${icon('usb')}USB</span>`);
  chips.push(
    `<span class="chip ${h.wifi ? 'on' : ''}" title="${h.wifi ? `Connected over Wi-Fi (${esc(h.wifi)})` : h.ip ? `Last known IP ${esc(h.ip)}` : 'No Wi-Fi connection'}">${icon('wifi')}${h.wifi ? esc(h.ip || 'Wi-Fi') : 'Wi-Fi'}</span>`,
  );
  chips.push(
    h.trusted
      ? `<span class="chip verified" title="Verified over USB">${icon('shield')}Verified</span>`
      : `<span class="chip unverified" title="Not verified yet">${icon('alert')}Unverified</span>`,
  );

  let primary = '';
  if (h.status === 'offline') {
    primary = `<span class="muted small">Offline${h.lastSeen ? ` · seen ${timeAgo(h.lastSeen)}` : ''}</span>`;
  } else if (!h.trusted && h.usb) {
    primary = `<button class="btn primary sm" data-action="verify" data-id="${esc(h.id)}">${icon('shield')}Verify</button>`;
  } else if (!h.trusted) {
    primary = `<span class="muted small">Connect via USB once to verify</span>`;
    if (h.canCast) primary += `<button class="btn sm" data-action="cast" data-id="${esc(h.id)}">${icon('play')}Cast</button>`;
  } else if (streaming) {
    primary = `<button class="btn sm danger" data-action="stop" data-id="${esc(h.id)}">${icon('stop')}Stop</button>`;
  } else {
    primary = `<button class="btn primary sm" data-action="cast" data-id="${esc(h.id)}">${icon('play')}Cast</button>`;
  }

  let wifiBtn = '';
  if (h.trusted && h.usb && !h.wifi) wifiBtn = `<button class="btn sm ghost" data-action="wifi" data-id="${esc(h.id)}" title="Enable casting over Wi-Fi">${icon('wifi')}Enable Wi-Fi</button>`;
  if (S.env.remote && !h.trusted && h.usb && h.status !== 'offline') primary = `<span class="muted small">${icon('shield')} Verify at the hub computer</span>`;
  if (!canControl() && h.status !== 'offline') {
    primary = streaming ? '' : `<span class="muted small">View only</span>`;
    wifiBtn = '';
  }

  return `
    <div class="hs-card ${h.status} ${streaming ? 'streaming' : ''}" style="--hs-color:${esc(h.color)}" data-id="${esc(h.id)}">
      <div class="hs-top">
        <span class="hs-dot"></span>
        ${h.index ? `<span class="hs-index">#${h.index}</span>` : ''}
        <div class="hs-title">
          <b title="${esc(h.name)}">${esc(h.name)}</b>
          <small><span class="brand-pill" style="--brand-color:${b.color}">${esc(b.name)}</span> ${esc(h.model)}</small>
        </div>
        ${batteryHtml(h.battery)}
        ${h.trusted && canControl() ? `<button class="btn icon ghost sm" data-action="menu" data-id="${esc(h.id)}" title="More">${icon('more')}</button>` : ''}
      </div>
      <div class="hs-chips">${chips.join('')}</div>
      <div class="hs-actions">${primary}${wifiBtn}${streaming && st.state === 'live' ? '<span class="live-pill">LIVE</span>' : ''}</div>
    </div>`;
}

function timeAgo(ts) {
  const s = Math.round((Date.now() - ts) / 1000);
  if (s < 60) return 'just now';
  if (s < 3600) return `${Math.round(s / 60)} min ago`;
  if (s < 86400) return `${Math.round(s / 3600)} h ago`;
  return `${Math.round(s / 86400)} d ago`;
}

function renderSidebar() {
  const list = $('#headset-list');
  const online = S.headsets.filter((h) => !h.pending && h.status === 'online');
  const pending = S.headsets.filter((h) => h.pending);
  const offline = S.headsets.filter((h) => !h.pending && h.status === 'offline');
  const section = (title, items) => (items.length ? `<div class="list-section"><h4>${title}</h4>${items.map(headsetCard).join('')}</div>` : '');
  list.innerHTML =
    section('Needs attention', pending) +
    section('Connected', online) +
    section('Saved · offline', offline) +
    (S.headsets.length ? '' : `<div class="list-empty">${icon('headset')}<p>No headsets yet.<br/>Plug one in via USB.</p></div>`);
  $('#hs-count').textContent = online.length ? `${online.length} online` : '';
  const err = $('#adb-error');
  err.classList.toggle('hidden', !S.adbError);
  err.textContent = S.adbError ? `adb: ${S.adbError}` : '';
  updateEmptyState();
}

function onDevices(snap) {
  S.headsets = snap.headsets;
  S.adbError = snap.adbError;
  renderSidebar();
  for (const t of S.tiles.values()) t.refresh();
}

// ----------------------------------------------------------- action menus

function openMenu(anchor, id) {
  closeMenu();
  const h = headsetById(id);
  if (!h) return;
  const items = [
    { a: 'edit', label: 'Rename / colour' },
    { a: 'calibrate', label: 'Calibrate lens correction' },
    h.usb && h.wifi ? { a: 'cast-usb', label: 'Cast via USB' } : null,
    h.wifi ? { a: 'cast-wifi', label: 'Cast via Wi-Fi' } : null,
    h.usb && !h.wifi ? { a: 'wifi', label: 'Enable Wi-Fi casting' } : null,
    h.wifi ? { a: 'wifi-off', label: 'Disconnect Wi-Fi' } : null,
    S.env.remote ? null : { a: 'forget', label: 'Forget headset', danger: true },
  ].filter(Boolean);
  const menu = document.createElement('div');
  menu.className = 'menu';
  menu.innerHTML = items.map((i) => `<button class="${i.danger ? 'danger' : ''}" data-action="${i.a}" data-id="${esc(id)}">${esc(i.label)}</button>`).join('');
  document.body.appendChild(menu);
  const r = anchor.getBoundingClientRect();
  menu.style.top = `${Math.min(window.innerHeight - menu.offsetHeight - 8, r.bottom + 4)}px`;
  menu.style.left = `${Math.max(8, r.right - menu.offsetWidth)}px`;
  setTimeout(() => document.addEventListener('mousedown', closeMenuOnOutside), 0);
}

function closeMenuOnOutside(e) {
  if (!e.target.closest('.menu')) closeMenu();
}

function closeMenu() {
  $$('.menu').forEach((m) => m.remove());
  document.removeEventListener('mousedown', closeMenuOnOutside);
}

function colorSwatches(selected) {
  return `<div class="swatches">${S.colors
    .map((c) => `<button type="button" class="swatch ${c === selected ? 'active' : ''}" data-color="${c}" style="--c:${c}" title="${c}"></button>`)
    .join('')}</div>`;
}

function bindSwatches(root, onPick) {
  root.addEventListener('click', (e) => {
    const s = e.target.closest('.swatch');
    if (!s) return;
    $$('.swatch', root).forEach((x) => x.classList.toggle('active', x === s));
    onPick(s.dataset.color);
  });
}

function openVerify(id) {
  const h = headsetById(id);
  if (!h) return;
  const b = brandOf(h);
  const used = new Set(S.headsets.filter((x) => x.trusted).map((x) => x.color));
  let color = S.colors.find((c) => !used.has(c)) || S.colors[0];
  const body = document.createElement('div');
  body.innerHTML = `
    <div class="verify-device" style="--brand-color:${b.color}">
      <div class="verify-icon">${icon('headset')}</div>
      <div>
        <b>${esc(b.family)} — ${esc(h.model)}</b>
        <div class="muted small">${[`Serial ${h.serial}`, h.manufacturer, h.androidVersion && `Android ${h.androidVersion}`].filter(Boolean).map(esc).join(' · ')}</div>
      </div>
    </div>
    <p class="muted">Verifying confirms this exact headset (by hardware serial) is allowed to cast to this computer. Give it a clear label — it is shown on its stream at all times.</p>
    <label class="field"><span>Label</span><input id="v-name" maxlength="40" placeholder="e.g. Station 3, Trainee Alex" value="" /></label>
    <label class="field"><span>Colour</span>${colorSwatches(color)}</label>
    <label class="check"><input type="checkbox" id="v-wifi" checked /> <span>Enable Wi-Fi casting now (you can unplug the cable afterwards)</span></label>`;
  bindSwatches(body, (c) => (color = c));
  openModal({
    title: 'Verify headset',
    body,
    actions: [
      { label: 'Cancel' },
      {
        label: 'Verify & save',
        kind: 'primary',
        onClick: async (el) => {
          const name = $('#v-name', el).value.trim();
          const enableWireless = $('#v-wifi', el).checked;
          const res = await run(xr.verify(id, { name, color, enableWireless }));
          return res ? false : true;
        },
      },
    ],
  });
}

function openEdit(id) {
  const h = headsetById(id);
  if (!h) return;
  let color = h.color;
  const body = document.createElement('div');
  body.innerHTML = `
    <label class="field"><span>Label</span><input id="e-name" maxlength="40" value="${esc(h.name)}" /></label>
    <label class="field"><span>Colour</span>${colorSwatches(color)}</label>
    <label class="field"><span>Preferred connection</span>
      <select id="e-transport">
        <option value="auto">Automatic (USB when plugged in, else Wi-Fi)</option>
        <option value="usb">USB only</option>
        <option value="wifi">Wi-Fi only</option>
      </select>
    </label>
    <div class="field"><span>Video encoders on this headset</span><div id="e-encoders" class="muted small">${h.status === 'online' ? 'Checking…' : 'Connect the headset to check.'}</div></div>
    <p class="muted small">Serial ${esc(h.serial)} · verified ${h.verifiedAt ? new Date(h.verifiedAt).toLocaleString() : '—'}</p>`;
  $('#e-transport', body).value = h.transport || 'auto';
  xr.encoders(id)
    .then((r) => {
      const el = $('#e-encoders', body);
      if (!r.encoders || !r.encoders.length) {
        el.textContent = r.live ? 'None reported.' : 'Not checked yet — connect the headset.';
        return;
      }
      el.innerHTML = Object.keys(S.codecs)
        .map((c) => {
          const encs = r.encoders.filter((e) => e.codec === c && !e.alias);
          const name = S.codecs[c].label;
          if (!encs.length) return `<div>${esc(name)}: <span class="bad">not available</span></div>`;
          const hw = encs.filter((e) => e.hw);
          return `<div>${esc(name)}: <b>${hw.length ? 'hardware' : 'software only'}</b> <span class="muted">(${encs.map((e) => esc(e.name)).join(', ')})</span></div>`;
        })
        .join('');
    })
    .catch((e) => ($('#e-encoders', body).textContent = `Could not check: ${e.message}`));
  bindSwatches(body, (c) => (color = c));
  openModal({
    title: `Edit ${h.name}`,
    body,
    actions: [
      { label: 'Cancel' },
      {
        label: 'Save',
        kind: 'primary',
        onClick: async (el) => {
          const name = $('#e-name', el).value.trim() || h.name;
          const res = await run(xr.update(id, { name, color, transport: $('#e-transport', el).value }));
          return res ? false : true;
        },
      },
    ],
  });
}

function confirmForget(id) {
  const h = headsetById(id);
  if (!h) return;
  openModal({
    title: 'Forget headset?',
    body: `<p><b>${esc(h.name)}</b> will be removed from this computer. It must be verified over USB again before it can cast.</p>`,
    actions: [{ label: 'Cancel' }, { label: 'Forget', kind: 'danger', onClick: () => run(xr.forget(id), `${h.name} forgotten.`) }],
  });
}

function openAddIp() {
  openModal({
    title: 'Connect by IP address',
    body: `
      <p class="muted">The headset must already be in network-debugging mode (enable Wi-Fi casting for it once via USB, or turn on Wireless debugging on Android 11+ headsets that support it).</p>
      <label class="field"><span>Address</span><input id="ip-addr" placeholder="192.168.1.50 or 192.168.1.50:5555" /></label>`,
    actions: [
      { label: 'Cancel' },
      {
        label: 'Connect',
        kind: 'primary',
        onClick: async (el) => {
          const v = $('#ip-addr', el).value.trim();
          if (!v) return true;
          const res = await run(xr.connect(v).then(() => true), `Connected to ${v}`);
          return res ? false : true;
        },
      },
    ],
  });
}

async function doScan(btn) {
  if (btn) {
    btn.disabled = true;
    btn.classList.add('busy');
  }
  toast('Scanning the local network for headsets…', 'info', 3000);
  const r = await run(xr.scan());
  if (btn) {
    btn.disabled = false;
    btn.classList.remove('busy');
  }
  if (!r) return;
  const parts = [`Found ${r.probed} device${r.probed === 1 ? '' : 's'} with ADB open`];
  if (r.connected) parts.push(`${r.connected} new connection${r.connected === 1 ? '' : 's'}`);
  if (r.unverified) parts.push(`${r.unverified} unverified (connect via USB to verify)`);
  if (r.pendingAuth) parts.push(`${r.pendingAuth} waiting for permission in the headset`);
  toast(parts.join(' · '), r.connected ? 'ok' : 'info', 7000);
}

// ---------------------------------------------------------------- guide

function openGuide(brandId) {
  const ids = ['meta', 'pico', 'yvr', 'htc', 'androidxr'];
  let active = brandId && ids.includes(brandId) ? brandId : 'meta';
  const body = document.createElement('div');
  const platformNotes = {
    linux: `<li><b>Linux:</b> if the headset shows as <i>no permissions</i>, add udev rules (e.g. install the <code>android-sdk-platform-tools-common</code> / <code>android-udev</code> package) and make sure your user is in the <code>plugdev</code> group, then re-plug.</li>`,
    win32: `<li><b>Windows:</b> Meta Quest needs the <i>Oculus ADB Drivers</i>; other brands normally work with the <i>Google USB Driver</i> or the vendor's driver. Re-plug after installing.</li>`,
    darwin: `<li><b>macOS:</b> no driver needed. Use a USB-C <i>data</i> cable (some charge-only cables won't work).</li>`,
  };
  const render = () => {
    const b = S.brands[active];
    body.innerHTML = `
      <div class="tabs">${ids.map((id) => `<button class="tab ${id === active ? 'active' : ''}" data-tab="${id}" style="--brand-color:${S.brands[id].color}">${esc(S.brands[id].name)}</button>`).join('')}</div>
      <div class="guide-grid">
        <section>
          <h4>1 · Enable developer mode on ${esc(b.family)}</h4>
          <ol>${b.setup.map((s) => `<li>${esc(s)}</li>`).join('')}</ol>
          <p class="muted small">Menu names vary slightly between firmware versions.</p>
        </section>
        <section>
          <h4>2 · Verify over USB</h4>
          <ol>
            <li>With the headset plugged in, it appears in the left panel under <b>Connected</b> as <span class="chip unverified">${icon('alert')}Unverified</span>.</li>
            <li>Click <b>Verify</b>, give it a clear label and colour. Keep <i>Enable Wi-Fi casting</i> ticked.</li>
            <li>When you see <i>Wi-Fi casting enabled</i>, you can unplug the cable.</li>
          </ol>
          <h4>3 · Cast</h4>
          <ol>
            <li>Click <b>Cast</b> on a headset or <b>Cast all</b> in the toolbar.</li>
            <li>Streams are lens-corrected by default. Use <b>Calibrate</b> (${icon('sliders')}) on a stream to fine-tune for a specific headset.</li>
          </ol>
        </section>
      </div>
      <h4>Troubleshooting</h4>
      <ul class="trouble">
        ${platformNotes[S.env.platform] || ''}
        <li><b>Wi-Fi casting stops after the headset reboots.</b> Network ADB is reset on reboot — plug in via USB once and click <i>Enable Wi-Fi</i> again.</li>
        <li><b>Scan finds nothing.</b> The PC and headsets must be on the same network/subnet; guest networks and some enterprise Wi-Fi block device-to-device traffic (client isolation).</li>
        <li><b>"adb server version doesn't match".</b> Another tool (SideQuest, Android Studio, Meta Quest Developer Hub) is running a different adb. Close it, or point XR MultiCast at the same adb with the <code>XRMC_ADB</code> environment variable.</li>
        <li><b>Black video.</b> Wake the headset (press the power button) — the display must be on to be mirrored.</li>
      </ul>`;
    hydrateIcons(body);
  };
  body.addEventListener('click', (e) => {
    const t = e.target.closest('[data-tab]');
    if (t) {
      active = t.dataset.tab;
      render();
    }
  });
  render();
  openModal({ title: 'Connecting headsets', body, wide: true, actions: [{ label: 'Close', kind: 'primary' }] });
}

// ------------------------------------------------------------- settings

const MAX_SIZE_OPTIONS = [
  [1280, '1280 px'],
  [1600, '1600 px'],
  [1920, '1920 px'],
  [2560, '2560 px'],
  [3200, '3200 px'],
  [0, 'Full resolution'],
];
const FPS_OPTIONS = [30, 45, 60, 72, 90, 120];

function decoderSummary() {
  const parts = Object.entries(S.codecs).map(([k, c]) => {
    const d = S.decoderSupport[k];
    const name = c.label.replace(/ \(.*\)/, '');
    if (!d || !d.any) return `<span class="bad">${esc(name)} ✗</span>`;
    return `${esc(name)} ${d.hw ? '(hardware)' : '(software)'}`;
  });
  return parts.join(', ') || 'unknown';
}

function tenBitSummary() {
  const parts = [
    ['h265_10', 'H.265 10-bit'],
    ['av1_10', 'AV1 10-bit'],
  ].map(([k, name]) => {
    const d = S.decoderSupport[k];
    return d && d.any ? `${name} ${d.hw ? '(hardware)' : '(software)'}` : `<span class="bad">${name} ✗</span>`;
  });
  return parts.join(', ');
}

function videoBlock(kind, label, s) {
  const q = s[`${kind}Quality`];
  const codec = s[`${kind}Codec`] || 'auto';
  const custom = s[`${kind}Custom`] || {};
  const presetOptions =
    Object.entries(S.presets)
      .map(([k, p]) => `<option value="${k}" ${k === q ? 'selected' : ''}>${esc(p.label)}</option>`)
      .join('') + `<option value="custom" ${q === 'custom' ? 'selected' : ''}>Custom…</option>`;
  const codecOptions =
    `<option value="auto" ${codec === 'auto' ? 'selected' : ''}>Auto (best available)</option>` +
    Object.entries(S.codecs)
      .map(([k, c]) => `<option value="${k}" ${k === codec ? 'selected' : ''} title="${esc(c.description)}">${esc(c.label)}</option>`)
      .join('');
  return `
    <div class="video-block" data-kind="${kind}">
      <div class="field-row">
        <label class="field"><span>Quality over ${label}</span><select id="s-${kind}q" data-quality="${kind}">${presetOptions}</select></label>
        <label class="field"><span>Codec over ${label}</span><select id="s-${kind}codec">${codecOptions}</select></label>
      </div>
      <div class="field-row custom-row" ${q === 'custom' ? '' : 'hidden'}>
        <label class="field"><span>Max resolution</span><select id="s-${kind}size">${MAX_SIZE_OPTIONS.map(([v, l]) => `<option value="${v}" ${Number(custom.maxSize) === v ? 'selected' : ''}>${l}</option>`).join('')}</select></label>
        <label class="field"><span>Bitrate (Mbps)</span><input id="s-${kind}mbps" type="number" min="0.5" max="200" step="0.5" value="${esc(custom.bitRateMbps || 8)}"/></label>
        <label class="field"><span>Max fps</span><select id="s-${kind}fps">${FPS_OPTIONS.map((v) => `<option value="${v}" ${Number(custom.maxFps) === v ? 'selected' : ''}>${v}</option>`).join('')}</select></label>
      </div>
    </div>`;
}

function readVideoBlock(el, kind) {
  return {
    [`${kind}Quality`]: $(`#s-${kind}q`, el).value,
    [`${kind}Codec`]: $(`#s-${kind}codec`, el).value,
    [`${kind}Custom`]: {
      maxSize: Number($(`#s-${kind}size`, el).value),
      bitRateMbps: Math.min(200, Math.max(0.5, Number($(`#s-${kind}mbps`, el).value) || 8)),
      maxFps: Number($(`#s-${kind}fps`, el).value),
    },
  };
}

function remoteBlock(s) {
  return `
    <h4>Remote access (tablets &amp; phones)</h4>
    <label class="check"><input type="checkbox" id="s-remote" ${s.remoteEnabled ? 'checked' : ''}/> <span><b>Let tablets and phones on this network open XR MultiCast in a browser</b><br/><small class="muted">They see the same dashboard, with lens-corrected video. Each device enters the PIN below once. Applies when you click Save.</small></span></label>
    <label class="field remote-port"><span>Port</span><input id="s-rport" type="number" min="1024" max="65535" value="${esc(s.remotePort || 8787)}"/></label>
    <label class="check"><input type="checkbox" id="s-rcontrol" ${s.remoteAllowControl ? 'checked' : ''}/> <span><b>Allow remote control</b><br/><small class="muted">Remote devices may start and stop casting, scan, enable Wi-Fi, rename, recolour and calibrate. Off = view only. Verifying and forgetting headsets and these settings are always hub-only.</small></span></label>
    <div id="s-remote-live" class="remote-live"><span class="muted small">Loading…</span></div>`;
}

function readRemoteBlock(el) {
  return {
    remoteEnabled: $('#s-remote', el).checked,
    remotePort: Math.round(Number($('#s-rport', el).value)) || 8787,
    remoteAllowControl: $('#s-rcontrol', el).checked,
  };
}

function bindRemoteBlock(body) {
  const live = $('#s-remote-live', body);
  let qrIndex = 0;
  let info = null;
  const draw = () => {
    if (!info) return;
    if (info.failed) {
      live.innerHTML = `<span class="bad small">${esc(info.failed)}</span>`;
      return;
    }
    const status = info.error
      ? `<span class="bad">${icon('alert')} ${esc(info.error)}</span>`
      : info.running
        ? `<span class="ok-text">${icon('check')} Running on port ${info.port} · ${info.viewers} viewer${info.viewers === 1 ? '' : 's'} connected</span>`
        : `<span class="muted">${info.enabled ? 'Starting…' : 'Off. Tick the box above and Save to start.'}</span>`;
    const url = info.urls[Math.min(qrIndex, info.urls.length - 1)];
    const access = info.running
      ? `<div class="remote-access">
          <div class="remote-qr">${url ? info.qrs[url] : ''}</div>
          <div class="remote-urls">
            <span class="muted small">On the tablet or phone, open:</span>
            ${info.urls.map((u, i) => `<button type="button" class="remote-url ${i === qrIndex ? 'active' : ''}" data-remote="qr" data-idx="${i}" title="Show QR code for this address">${esc(u)}</button>`).join('') || '<span class="bad small">No network connection.</span>'}
            <span class="muted small">Scan the QR code with the camera, then "Add to Home Screen" to use it like an app.</span>
          </div>
        </div>`
      : '';
    const paired = info.paired.length
      ? info.paired
          .sort((a, b) => b.connected - a.connected || b.lastSeen - a.lastSeen)
          .map(
            (p) => `<div class="remote-device">
              <span class="hs-dot ${p.connected ? 'on' : ''}"></span>
              <div><b>${esc(p.name)}</b><small class="muted">${p.connected ? 'Connected' : `Last seen ${timeAgo(p.lastSeen)}`}${p.ip ? ` · ${esc(p.ip)}` : ''}</small></div>
              <button type="button" class="btn sm ghost danger" data-remote="revoke" data-rid="${esc(p.id)}">Revoke</button>
            </div>`,
          )
          .join('')
      : '<p class="muted small">No paired devices yet.</p>';
    live.innerHTML = `
      <p class="small">${status}</p>
      ${access}
      <div class="remote-pin"><span class="muted small">Pairing PIN</span><b>${esc(info.pin)}</b><button type="button" class="btn sm" data-remote="newpin" title="Make a new PIN (already paired devices stay paired)">${icon('refresh')}New PIN</button></div>
      <div class="remote-devices-head"><span class="muted small">Paired devices</span>${info.paired.length > 1 ? '<button type="button" class="btn sm ghost danger" data-remote="revoke" data-rid="all">Revoke all</button>' : ''}</div>
      ${paired}`;
  };
  const load = () =>
    xr
      .remoteInfo()
      .then((i) => (info = i), (e) => (info = { failed: e.message }))
      .then(draw);
  live.addEventListener('click', async (e) => {
    const b = e.target.closest('[data-remote]');
    if (!b) return;
    const act = b.dataset.remote;
    if (act === 'qr') {
      qrIndex = Number(b.dataset.idx) || 0;
      draw();
    } else if (act === 'newpin') await run(xr.remoteNewPin());
    else if (act === 'revoke') {
      const n = await run(xr.remoteRevoke(b.dataset.rid));
      if (n != null) toast(b.dataset.rid === 'all' ? 'All remote devices unpaired.' : 'Device unpaired.', 'ok');
    }
    load();
  });
  const off = xr.onRemoteStatus(() => {
    if (!live.isConnected) return off();
    load();
  });
  load();
}

/** Settings dialog in a remote browser: per-device preferences and unpairing; the rest lives on the hub. */
function openRemoteSettings() {
  const body = document.createElement('div');
  body.className = 'settings';
  body.innerHTML = `
    <h4>This device</h4>
    <p class="muted">Connected to the XR MultiCast hub at <b>${esc(location.host)}</b> with <b>${canControl() ? 'control' : 'view-only'}</b> access. Video: ${S.env.webCodecs ? 'WebCodecs' : 'Media Source Extensions (H.264)'}.</p>
    <label class="field"><span>Default view on this device</span>
      <select id="s-view">${VIEW_MODES.map((m) => `<option value="${m.id}" ${m.id === S.settings.defaultViewMode ? 'selected' : ''}>${m.label} — ${esc(m.title)}</option>`).join('')}</select>
    </label>
    <p class="muted small">Casting quality, codecs, security and remote access are set on the hub computer. Headsets are verified there too, over USB.</p>
    <h4>Pairing</h4>
    <p class="muted small">Unpairing removes this device from the hub. You will need the PIN again to reconnect.</p>`;
  openModal({
    title: 'Settings',
    body,
    actions: [
      { label: 'Unpair this device', kind: 'danger', onClick: () => xr.unpair() },
      { label: 'Close' },
      {
        label: 'Save',
        kind: 'primary',
        onClick: async (el) => {
          const res = await run(xr.updateSettings({ defaultViewMode: $('#s-view', el).value }), 'Saved on this device.');
          if (res) Object.assign(S.settings, res);
        },
      },
    ],
  });
}

function openSettings() {
  if (S.env.remote) return openRemoteSettings();
  const s = S.settings;
  const body = document.createElement('div');
  body.className = 'settings';
  body.addEventListener('change', (e) => {
    const kind = e.target.dataset.quality;
    if (kind) $(`.video-block[data-kind="${kind}"] .custom-row`, body).hidden = e.target.value !== 'custom';
  });
  body.innerHTML = `
    <h4>Security</h4>
    <label class="check"><input type="checkbox" id="s-require" ${s.requireUsbVerification ? 'checked' : ''}/> <span><b>Only cast headsets verified over USB</b><br/><small class="muted">Recommended. Unverified devices found on the network are listed but cannot cast.</small></span></label>
    <h4>Connections</h4>
    <label class="check"><input type="checkbox" id="s-reconnect" ${s.autoReconnectWifi ? 'checked' : ''}/> <span>Automatically reconnect verified headsets over Wi-Fi</span></label>
    <label class="check"><input type="checkbox" id="s-autocast" ${s.autoCastOnConnect ? 'checked' : ''}/> <span>Start casting automatically when a verified headset connects</span></label>
    <label class="check"><input type="checkbox" id="s-retry" ${s.autoRetryStreams ? 'checked' : ''}/> <span>Retry streams automatically if they drop</span></label>
    <label class="field"><span>Network scan port</span><input id="s-port" type="number" min="1" max="65535" value="${esc(s.scanPort)}"/></label>
    <h4>Video</h4>
    ${videoBlock('usb', 'USB', s)}
    ${videoBlock('wifi', 'Wi-Fi', s)}
    <p class="muted small">${Object.values(S.codecs).map((c) => `<b>${esc(c.label)}</b>: ${esc(c.description)}.`).join(' ')}</p>
    <label class="check"><input type="checkbox" id="s-tenbit" ${s.tenBit ? 'checked' : ''}/> <span><b>10-bit colour</b> <span class="tag">Experimental</span><br/><small class="muted">Asks the headset for 10-bit H.265 (Main 10) or AV1 when one of those codecs is used. Headset compositors output 8-bit, so the gain is smoother gradients (less banding) rather than more colour. Many headset encoders refuse 10-bit; the stream then falls back to 8-bit automatically. This computer: ${tenBitSummary()}.</small></span></label>
    <p class="muted small">This computer can decode: ${decoderSummary()}. <b>Auto</b> uses H.265, then AV1, only when the headset has a hardware encoder for it and this computer can hardware-decode it; otherwise H.264. A stream that fails to start or decode falls back to H.264 automatically. Changes apply to streams started afterwards.</p>
    <label class="field"><span>Default view</span>
      <select id="s-view">${VIEW_MODES.map((m) => `<option value="${m.id}" ${m.id === s.defaultViewMode ? 'selected' : ''}>${m.label} — ${esc(m.title)}</option>`).join('')}</select>
    </label>
    <label class="check"><input type="checkbox" id="s-oneeye" ${s.singleEyeStream ? 'checked' : ''}/> <span><b>Stream one eye only</b><br/><small class="muted">The headset crops to the selected eye before encoding: about twice the sharpness at the same bandwidth. "Both eyes" view is unavailable while on. Applies to new streams.</small></span></label>
    ${remoteBlock(s)}
    <h4>About</h4>
    <p class="muted small">XR MultiCast ${esc(S.env.appVersion)} · adb ${esc(S.env.adbVersion || 'not found')} (${esc(S.env.adbPath)}) · scrcpy-server ${esc(S.env.scrcpyVersion)} ${S.env.serverAvailable ? '✓' : '— missing, run "npm run setup"'}</p>`;
  bindRemoteBlock(body);
  openModal({
    title: 'Settings',
    body,
    wide: true,
    actions: [
      { label: 'Cancel' },
      {
        label: 'Save',
        kind: 'primary',
        onClick: async (el) => {
          const patch = {
            requireUsbVerification: $('#s-require', el).checked,
            autoReconnectWifi: $('#s-reconnect', el).checked,
            autoCastOnConnect: $('#s-autocast', el).checked,
            autoRetryStreams: $('#s-retry', el).checked,
            scanPort: Number($('#s-port', el).value) || 5555,
            ...readVideoBlock(el, 'usb'),
            ...readVideoBlock(el, 'wifi'),
            tenBit: $('#s-tenbit', el).checked,
            defaultViewMode: $('#s-view', el).value,
            singleEyeStream: $('#s-oneeye', el).checked,
          };
          Object.assign(patch, readRemoteBlock(el));
          const res = await run(xr.updateSettings(patch), 'Settings saved.');
          if (res) S.settings = res;
          return res ? false : true;
        },
      },
    ],
  });
}

// ---------------------------------------------------------- calibration

const CALIB_SLIDERS = [
  { key: 'k1', label: 'Barrel correction (k1)', min: -0.6, max: 0.3, step: 0.005, hint: 'Main fisheye removal. More negative = straighter edges.' },
  { key: 'k2', label: 'Edge correction (k2)', min: -0.3, max: 0.3, step: 0.005, hint: 'Fine-tunes the outer edge curvature.' },
  { key: 'zoom', label: 'Zoom', min: 0.6, max: 2.5, step: 0.01, hint: 'Crop into the centre of the view.' },
  { key: 'centerX', label: 'Lens centre X', min: -0.35, max: 0.35, step: 0.005, hint: 'Shift toward the nose / temple. Mirrored for the right eye.' },
  { key: 'centerY', label: 'Lens centre Y', min: -0.35, max: 0.35, step: 0.005 },
  { key: 'chromaR', label: 'Red fringe', min: 0.97, max: 1.03, step: 0.0005, hint: 'Removes red/cyan colour fringing at the edges.' },
  { key: 'chromaB', label: 'Blue fringe', min: 0.97, max: 1.03, step: 0.0005, hint: 'Removes blue/yellow colour fringing at the edges.' },
];

function openCalibration(id) {
  const h = headsetById(id);
  const t = S.tiles.get(id);
  if (!h || !t) return;
  if (t.viewMode === 'raw') {
    t.viewMode = 'corrected';
    t.refresh();
  }
  const params = { ...correctionFor(h) };
  if (t.eyeOverride) params.eye = t.eyeOverride;
  if (t.rotationOverride != null) params.rotation = t.rotationOverride;
  S.calib = { id, params, grid: true, original: { ...params } };
  setFocus(id);
  renderCalibration();
  $('#calib').classList.remove('hidden');
}

/** Labels for the 'auto' layout/rotation options, showing what auto currently resolves to. */
function calibrationAuto() {
  const c = S.calib;
  const t = c && S.tiles.get(c.id);
  if (!t || !t.renderer || !t.renderer.hasFrame) return { layout: 'Auto detect', rotation: 'Auto' };
  const lay = t.resolve({ ...c.params, layout: 'auto' });
  const rot = t.resolve({ ...c.params, rotation: 'auto' });
  return { layout: `Auto (${LAYOUT_LABELS[lay.layout]})`, rotation: `Auto (${rot.rotation}°)` };
}

function syncCalibrationAuto() {
  const auto = calibrationAuto();
  const lay = $('#calib [data-k="layout"] option[value="auto"]');
  const rot = $('#calib [data-k="rotation"] option[value="auto"]');
  if (lay) lay.textContent = auto.layout;
  if (rot) rot.textContent = auto.rotation;
}

function renderCalibration() {
  const c = S.calib;
  const drawer = $('#calib');
  if (!c) {
    drawer.classList.add('hidden');
    drawer.innerHTML = '';
    return;
  }
  const h = headsetById(c.id);
  const b = brandOf(h);
  const p = c.params;
  const auto = calibrationAuto();
  const sel = (key, opts) => `<select data-k="${key}">${opts.map(([v, l]) => `<option value="${v}" ${String(p[key]) === String(v) ? 'selected' : ''}>${l}</option>`).join('')}</select>`;
  drawer.innerHTML = `
    <header class="drawer-head" style="--hs-color:${esc(h ? h.color : b.color)}">
      <div><small class="muted">Lens calibration</small><b>${esc(labelOf(h))}</b></div>
      <button class="btn icon ghost" data-cal="cancel" title="Close">${icon('x')}</button>
    </header>
    <div class="drawer-body">
      <p class="muted small">Look at something with straight lines in the headset (a door frame, a window). Adjust until those lines are straight in the preview. Starting values come from the <b>${esc(b.family)}</b> profile.</p>
      <label class="check"><input type="checkbox" data-cal="grid" ${c.grid ? 'checked' : ''}/> <span>Show reference grid</span></label>
      <div class="field-row">
        <label class="field"><span>Panel layout</span>${sel('layout', Object.entries(LAYOUT_LABELS).map(([k, l]) => [k, k === 'auto' ? auto.layout : l]))}</label>
        <label class="field"><span>Eye</span>${sel('eye', [['left', 'Left'], ['right', 'Right']])}</label>
      </div>
      <div class="field-row">
        <label class="field"><span>Output framing</span>${sel('outputAspect', [['16:9', '16:9'], ['4:3', '4:3'], ['1:1', 'Square'], ['eye', 'Full eye']])}</label>
        <label class="field"><span>Rotation</span>${sel('rotation', [['auto', auto.rotation], [0, '0°'], [90, '90°'], [180, '180°'], [270, '270°']])}</label>
      </div>
      <div class="field-row">
        <label class="field" title="Which row of a 2×2 grid holds the left/right eye pair"><span>Eye row (2×2 grid only)</span>${sel('quadRow', [['top', 'Top pair'], ['bottom', 'Bottom pair']])}</label>
      </div>
      <div class="calib-exp">
        <span class="calib-exp-head">Orientation <span class="tag">Experimental</span></span>
        <label class="check"><input type="checkbox" data-k="flipH" ${p.flipH ? 'checked' : ''}/> <span>Flip horizontally</span></label>
        <label class="check"><input type="checkbox" data-k="flipV" ${p.flipV ? 'checked' : ''}/> <span>Flip vertically</span></label>
      </div>
      ${CALIB_SLIDERS.map(
        (s) => `
        <label class="slider">
          <span class="slider-head"><span>${esc(s.label)}</span><output data-out="${s.key}">${Number(p[s.key]).toFixed(s.step < 0.001 ? 4 : 3)}</output></span>
          <input type="range" data-k="${s.key}" min="${s.min}" max="${s.max}" step="${s.step}" value="${p[s.key]}" />
          ${s.hint ? `<small class="muted">${esc(s.hint)}</small>` : ''}
        </label>`,
      ).join('')}
    </div>
    <footer class="drawer-foot">
      <button class="btn ghost" data-cal="reset" title="Back to the ${esc(b.family)} defaults">Reset to preset</button>
      <span class="spacer"></span>
      <button class="btn" data-cal="cancel">Cancel</button>
      <button class="btn primary" data-cal="save" ${h && h.trusted ? '' : 'disabled title="Verify the headset to save calibration"'}>Save</button>
    </footer>`;
}

function closeCalibration(save) {
  const c = S.calib;
  if (!c) return;
  S.calib = null;
  if (save) {
    const st = S.streams.get(c.id);
    run(xr.update(c.id, { calibration: c.params }), 'Calibration saved for this headset.').then((ok) => {
      if (ok && st && st.singleEye && c.params.eye !== c.original.eye) startStream(c.id, { restart: true });
    });
  }
  const t = S.tiles.get(c.id);
  if (t) {
    t.eyeOverride = c.params.eye;
    if (save) t.rotationOverride = null;
    t.refresh();
  }
  renderCalibration();
}

function bindCalibration() {
  const drawer = $('#calib');
  drawer.addEventListener('input', (e) => {
    const c = S.calib;
    if (!c) return;
    const k = e.target.dataset.k;
    if (k) {
      const numeric = e.target.type === 'range' || (k === 'rotation' && e.target.value !== 'auto');
      if (e.target.type === 'checkbox') c.params[k] = e.target.checked;
      else c.params[k] = numeric ? Number(e.target.value) : e.target.value;
      const out = $(`[data-out="${k}"]`, drawer);
      if (out) {
        const step = Number(e.target.step);
        out.textContent = Number(e.target.value).toFixed(step < 0.001 ? 4 : 3);
      }
    } else if (e.target.dataset.cal === 'grid') {
      c.grid = e.target.checked;
    }
    const t = S.tiles.get(c.id);
    if (t) {
      t.dirty = true;
      if (k === 'eye') t.refresh();
      if (k === 'layout' || k === 'rotation') {
        syncCalibrationAuto();
        t.updateStats();
      }
    }
  });
  drawer.addEventListener('click', (e) => {
    const a = e.target.closest('[data-cal]');
    if (!a || !S.calib || a.type === 'checkbox') return;
    const act = a.dataset.cal;
    if (act === 'save') closeCalibration(true);
    else if (act === 'cancel') closeCalibration(false);
    else if (act === 'reset') {
      const h = headsetById(S.calib.id);
      S.calib.params = { ...presetFor(h), eye: S.calib.params.eye };
      renderCalibration();
      const t = S.tiles.get(S.calib.id);
      if (t) t.dirty = true;
    }
  });
}

// ------------------------------------------------------------ main loop

function frameLoop(now) {
  for (const t of S.tiles.values()) {
    if (!t.el.hidden) t.tick(now);
    else if (t.pendingFrame) {
      t.pendingFrame.close();
      t.pendingFrame = null;
    }
  }
  requestAnimationFrame(frameLoop);
}

function renderStatusBar() {
  const e = S.env;
  $('#status-adb').innerHTML = `${icon('usb')} adb ${esc(e.adbVersion || 'not found')}`;
  $('#status-adb').title = e.adbPath;
  $('#status-server').innerHTML = e.serverAvailable
    ? `${icon('check')} scrcpy-server ${esc(e.scrcpyVersion)}`
    : `<span class="bad">${icon('alert')} scrcpy-server missing — run "npm run setup"</span>`;
  const sb = $('#status-sandbox');
  sb.hidden = e.sandboxed !== false;
  sb.innerHTML = `<span class="bad">${icon('alert')} Chromium sandbox off</span>`;
  sb.title =
    'Ubuntu blocks the sandbox until an AppArmor profile is installed. Run once in the project folder:\n' +
    '  bash packaging/linux/install-user.sh --system\n(or install the .deb package, which includes it)';
  $('#status-ip').innerHTML = e.localIps && e.localIps.length ? `${icon('wifi')} ${e.remote ? 'Hub' : 'This PC'}: ${e.localIps.map(esc).join(', ')}` : `${icon('wifi')} No network`;
  renderRemoteStatus();
}

function renderRemoteStatus(st) {
  const el = $('#status-remote');
  if (!el) return;
  if (S.env.remote) {
    el.hidden = false;
    el.innerHTML = `${icon('eye')} Remote viewer · ${canControl() ? 'control' : 'view only'}`;
    return;
  }
  if (st) S.remoteStatus = st;
  const r = S.remoteStatus;
  el.hidden = !r || !r.enabled;
  if (!r || !r.enabled) return;
  el.innerHTML = r.error ? `<span class="bad">${icon('alert')} Remote: ${esc(r.error)}</span>` : `${icon('eye')} Remote: ${r.viewers} viewer${r.viewers === 1 ? '' : 's'}`;
  el.title = 'Remote access for tablets & phones — open Settings';
}

function bindGlobal() {
  document.body.addEventListener('click', async (e) => {
    const el = e.target.closest('[data-action]');
    if (!el) return;
    e.preventDefault();
    const id = el.dataset.id;
    const a = el.dataset.action;
    closeMenu();
    // On narrow screens the sidebar is a drawer: get it out of the way once a stream is picked.
    if (['cast', 'cast-usb', 'cast-wifi', 'calibrate', 'edit', 'verify'].includes(a)) document.body.classList.remove('nav-open');
    switch (a) {
      case 'cast-all': {
        const ids = S.headsets.filter((h) => h.canCast && !S.streams.has(h.id)).map((h) => h.id);
        if (!ids.length) {
          toast(S.headsets.some((h) => h.status === 'online') ? 'No verified headsets to cast. Verify them over USB first.' : 'No headsets connected.', 'warn');
          break;
        }
        ids.forEach((hid) => startStream(hid));
        break;
      }
      case 'stop-all':
        await run(xr.stopAll());
        [...S.tiles.keys()].forEach(removeTile);
        break;
      case 'scan':
        doScan(el);
        break;
      case 'add-ip':
        openAddIp();
        break;
      case 'guide':
        openGuide();
        break;
      case 'settings':
        openSettings();
        break;
      case 'toggle-sidebar':
        document.body.classList.toggle('nav-open');
        break;
      case 'close-sidebar':
        document.body.classList.remove('nav-open');
        break;
      case 'verify':
        openVerify(id);
        break;
      case 'cast':
        startStream(id);
        break;
      case 'cast-usb':
        startStream(id, { transport: 'usb' });
        break;
      case 'cast-wifi':
        startStream(id, { transport: 'wifi' });
        break;
      case 'stop':
        stopStream(id);
        break;
      case 'wifi':
        toast('Enabling Wi-Fi casting… keep the cable plugged in for a few seconds.', 'info', 4000);
        run(xr.enableWireless(id));
        break;
      case 'wifi-off':
        run(xr.disconnectWireless(id), 'Wi-Fi connection closed.');
        break;
      case 'menu':
        openMenu(el, id);
        break;
      case 'edit':
        openEdit(id);
        break;
      case 'calibrate':
        if (!S.tiles.has(id)) await startStream(id);
        openCalibration(id);
        break;
      case 'forget':
        confirmForget(id);
        break;
      default:
        break;
    }
  });

  $('#grid-columns').addEventListener('change', (e) => {
    S.settings.gridColumns = e.target.value;
    xr.updateSettings({ gridColumns: e.target.value }).catch(() => {});
    layoutGrid();
  });

  document.addEventListener('keydown', (e) => {
    if (e.target.closest('input, select, textarea') || $('.modal-backdrop')) return;
    if (e.key === 'Escape') {
      if (S.calib) closeCalibration(false);
      else setFocus(null);
    } else if (/^[1-9]$/.test(e.key)) {
      const h = S.headsets.find((x) => x.index === Number(e.key));
      if (h && S.tiles.has(h.id)) setFocus(S.focus === h.id ? null : h.id);
    }
  });

  bindCalibration();
}

async function init() {
  hydrateIcons();
  bindGlobal();
  // Events can arrive before init() returns the brand tables needed to render them; hold them until then.
  let ready = false;
  const early = { snap: null, states: [] };
  xr.onDevices((snap) => (ready ? onDevices(snap) : (early.snap = snap)));
  xr.onNotice((n) => toast(n.message, n.level));
  xr.onStreamState((st) => (ready ? onStreamState(st) : early.states.push(st)));
  xr.onStreamLog((id, line) => console.debug(`[${id}] ${line}`));
  xr.onPacket((id, isConfig, isKey, pts, data) => {
    const t = S.tiles.get(id);
    if (t) t.push(isConfig, isKey, pts, data);
  });

  const init = await xr.init();
  S.brands = init.brands;
  S.colors = init.colors;
  S.presets = init.qualityPresets;
  S.codecs = init.codecs;
  probeDecoders()
    .then((support) => {
      S.decoderSupport = support;
      return xr.setDecoderSupport(support);
    })
    .catch((e) => console.warn('decoder probe failed', e));
  S.settings = init.settings;
  S.env = init.env;
  document.documentElement.classList.toggle('remote', !!S.env.remote);
  document.documentElement.classList.toggle('view-only', !canControl());
  if (!S.env.remote) {
    xr.onRemoteStatus(renderRemoteStatus);
    xr.remoteInfo().then(renderRemoteStatus, () => {});
  }
  $('#grid-columns').value = S.settings.gridColumns || 'auto';
  renderStatusBar();
  onDevices(early.snap || init.snapshot);
  for (const st of [...init.streams, ...early.states]) onStreamState(st);
  ready = true;
  layoutGrid();
  requestAnimationFrame(frameLoop);
  if (!S.env.serverAvailable) toast('scrcpy-server is missing — run "npm run setup" before casting.', 'error', 12000);
}

init().catch((e) => {
  document.body.innerHTML = `<pre class="fatal">Failed to start: ${esc(e.stack || e.message)}</pre>`;
});
