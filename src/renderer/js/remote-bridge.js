'use strict';

// Browser stand-in for src/preload.js: the same window.xr API, carried over a
// WebSocket to the XR MultiCast hub instead of Electron IPC. Only injected into the
// page when it is served by the hub's remote-access web server.

(function () {
  const TOKEN_KEY = 'xrmc.remoteToken';
  const LOCAL_KEY = 'xrmc.localSettings';
  const LOCAL_SETTINGS = ['gridColumns', 'defaultViewMode'];
  const HUB_ONLY = 'This can only be done on the hub computer.';

  const listeners = { devices: new Set(), notice: new Set(), state: new Set(), log: new Set(), packet: new Set() };
  const pending = new Map();
  const knownStreams = new Map();
  const textDecoder = new TextDecoder();
  let ws = null;
  let seq = 0;
  let retry = 0;
  let ready = false;
  let waiters = [];
  let initialized = false;
  let canControl = false;
  let support = null;
  let visible = [];
  let sentSubs = null;
  let overlay = null;
  let lockTimer = null;

  const store = {
    get(k) {
      try {
        return localStorage.getItem(k);
      } catch {
        return null;
      }
    },
    set(k, v) {
      try {
        if (v == null) localStorage.removeItem(k);
        else localStorage.setItem(k, v);
      } catch {
        /* private mode */
      }
    },
  };

  function loadLocal() {
    try {
      return JSON.parse(store.get(LOCAL_KEY) || '{}') || {};
    } catch {
      return {};
    }
  }

  const useWebCodecs = () => typeof window.VideoDecoder === 'function' && window.isSecureContext === true;

  async function getSupport() {
    if (support) return support;
    if (useWebCodecs() && typeof window.probeDecoders === 'function') {
      try {
        const s = await window.probeDecoders();
        if (s && s.h264) return (support = s);
      } catch {
        /* fall through to MSE */
      }
    }
    support = window.MseDecoder ? window.MseDecoder.support() : { h264: { any: true, hw: true } };
    return support;
  }

  function emit(event, args) {
    if (event === 'state' && args[0]) {
      if (args[0].state === 'stopped') knownStreams.delete(args[0].id);
      else knownStreams.set(args[0].id, args[0]);
    }
    for (const cb of listeners[event] || []) {
      try {
        cb(...args);
      } catch (e) {
        console.error(e);
      }
    }
  }

  function on(event) {
    return (cb) => {
      listeners[event].add(cb);
      return () => listeners[event].delete(cb);
    };
  }

  function sendJson(obj) {
    if (ws && ws.readyState === 1) ws.send(JSON.stringify(obj));
  }

  function whenReady() {
    return ready ? Promise.resolve() : new Promise((r) => waiters.push(r));
  }

  async function call(method, ...args) {
    await whenReady();
    const id = ++seq;
    return new Promise((resolve, reject) => {
      pending.set(id, { resolve, reject });
      sendJson({ t: 'call', id, method, args });
    });
  }

  // ------------------------------------------------------------ connection

  function connect() {
    const url = `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws`;
    ws = new WebSocket(url);
    ws.binaryType = 'arraybuffer';
    const sock = ws;
    const slow = setTimeout(() => {
      if (!ready && !overlay) showOverlay('connecting');
    }, 600);
    sock.onmessage = (e) => {
      if (typeof e.data === 'string') {
        let m;
        try {
          m = JSON.parse(e.data);
        } catch {
          return;
        }
        onMessage(m);
      } else onBinary(e.data);
    };
    sock.onclose = (e) => {
      clearTimeout(slow);
      if (sock !== ws) return;
      ready = false;
      sentSubs = null;
      for (const p of pending.values()) p.reject(new Error('Connection to the hub was lost.'));
      pending.clear();
      if (e.code === 4003) store.set(TOKEN_KEY, null);
      if (initialized) showBanner('Connection to the hub lost — reconnecting…');
      else if (!overlay || overlay.dataset.mode === 'connecting') showOverlay('connecting', 'Cannot reach the hub — retrying…');
      const delay = Math.min(10000, 500 * 2 ** retry++);
      setTimeout(connect, delay);
    };
  }

  async function onMessage(m) {
    switch (m.t) {
      case 'hello': {
        const token = store.get(TOKEN_KEY);
        const s = await getSupport();
        if (token) sendJson({ t: 'auth', token, support: s });
        else showOverlay('pair');
        break;
      }
      case 'auth-failed':
        store.set(TOKEN_KEY, null);
        showOverlay('pair', m.error);
        break;
      case 'pair-failed':
        pairFailed(m.error, m.retryAfter);
        break;
      case 'paired':
        store.set(TOKEN_KEY, m.token);
        break;
      case 'ready':
        if (initialized && m.canControl !== canControl) return location.reload();
        canControl = !!m.canControl;
        ready = true;
        retry = 0;
        hideOverlay();
        hideBanner();
        waiters.splice(0).forEach((r) => r());
        if (initialized) resync().catch((e) => console.warn('resync failed', e));
        break;
      case 'res': {
        const p = pending.get(m.id);
        if (!p) return;
        pending.delete(m.id);
        if (m.ok) p.resolve(m.value);
        else p.reject(new Error(m.error));
        break;
      }
      case 'error': {
        const p = pending.get(m.id);
        if (p) {
          pending.delete(m.id);
          p.reject(new Error(m.error));
        }
        break;
      }
      case 'event':
        if (listeners[m.event] && Array.isArray(m.args)) emit(m.event, m.args);
        break;
      case 'access':
        if (!!m.canControl !== canControl) location.reload();
        break;
      case 'revoked':
        store.set(TOKEN_KEY, null);
        break;
      default:
        break;
    }
  }

  function onBinary(buf) {
    const u8 = new Uint8Array(buf);
    if (u8.length < 10) return;
    const idLen = u8[1];
    const id = textDecoder.decode(u8.subarray(2, 2 + idLen));
    const pts = new DataView(buf).getFloat64(2 + idLen, true);
    emit('packet', [id, !!(u8[0] & 1), !!(u8[0] & 2), pts, u8.subarray(10 + idLen)]);
  }

  /** After a reconnect: bring the UI back in line with the hub. */
  async function resync() {
    const data = await call('app:init');
    emit('devices', [data.snapshot]);
    const live = new Set(data.streams.map((s) => s.id));
    for (const id of [...knownStreams.keys()]) if (!live.has(id)) emit('state', [{ id, state: 'stopped' }]);
    for (const s of data.streams) emit('state', [s]);
    pushSubscriptions();
  }

  function pushSubscriptions() {
    const ids = document.hidden ? [] : visible;
    const key = JSON.stringify(ids);
    if (!ready || key === sentSubs) return;
    sentSubs = key;
    sendJson({ t: 'subscribe', ids });
  }

  document.addEventListener('visibilitychange', pushSubscriptions);

  // ------------------------------------------------------------ UI helpers

  const LOGO =
    '<svg viewBox="0 0 32 32" class="logo" aria-hidden="true"><rect x="2" y="9" width="28" height="15" rx="6" fill="none" stroke="currentColor" stroke-width="2.4"/><circle cx="10.5" cy="16.5" r="3.2" fill="currentColor"/><circle cx="21.5" cy="16.5" r="3.2" fill="currentColor"/></svg>';

  function showOverlay(mode, message) {
    if (!overlay) {
      overlay = document.createElement('div');
      overlay.className = 'pair-overlay';
      document.body.appendChild(overlay);
    }
    overlay.dataset.mode = mode;
    clearInterval(lockTimer);
    if (mode === 'connecting') {
      overlay.innerHTML = `<div class="pair-card">${LOGO}<h1>XR MultiCast</h1><div class="spinner"></div><p class="muted"></p></div>`;
      overlay.querySelector('p').textContent = message || `Connecting to the hub at ${location.host}…`;
      return;
    }
    overlay.innerHTML = `
      <form class="pair-card" autocomplete="off">
        ${LOGO}
        <h1>Pair with XR MultiCast</h1>
        <p class="muted">Enter the 6-digit PIN shown on the hub computer under <b>Settings → Remote access</b>.</p>
        <input class="pair-pin" type="text" inputmode="numeric" pattern="[0-9]*" autocomplete="one-time-code" maxlength="6" placeholder="000000" aria-label="PIN" />
        <button class="btn primary pair-go" type="submit">Pair this device</button>
        <p class="pair-error" role="alert"></p>
        <p class="muted small">Hub: ${location.host}. This device stays paired until it is revoked on the hub.</p>
      </form>`;
    const form = overlay.querySelector('form');
    const input = overlay.querySelector('.pair-pin');
    if (message) overlay.querySelector('.pair-error').textContent = message;
    input.addEventListener('input', () => {
      input.value = input.value.replace(/\D/g, '').slice(0, 6);
      if (input.value.length === 6) form.requestSubmit ? form.requestSubmit() : submit();
    });
    const submit = async () => {
      const pin = input.value.trim();
      if (!/^\d{6}$/.test(pin)) {
        overlay.querySelector('.pair-error').textContent = 'The PIN has 6 digits.';
        return;
      }
      overlay.querySelector('.pair-go').disabled = true;
      overlay.querySelector('.pair-error').textContent = '';
      const hint = /Macintosh/.test(navigator.userAgent) && navigator.maxTouchPoints > 1 ? 'iPad' : '';
      sendJson({ t: 'pair', pin, hint, support: await getSupport() });
    };
    form.addEventListener('submit', (e) => {
      e.preventDefault();
      submit();
    });
    setTimeout(() => input.focus(), 50);
  }

  function pairFailed(error, retryAfter) {
    if (!overlay || overlay.dataset.mode !== 'pair') showOverlay('pair');
    const err = overlay.querySelector('.pair-error');
    const btn = overlay.querySelector('.pair-go');
    const input = overlay.querySelector('.pair-pin');
    err.textContent = error || 'Pairing failed.';
    input.value = '';
    btn.disabled = false;
    if (retryAfter > 0) {
      let left = retryAfter;
      btn.disabled = true;
      input.disabled = true;
      const tick = () => {
        btn.textContent = `Try again in ${left}s`;
        if (left-- <= 0) {
          clearInterval(lockTimer);
          btn.disabled = false;
          input.disabled = false;
          btn.textContent = 'Pair this device';
        }
      };
      tick();
      lockTimer = setInterval(tick, 1000);
    } else input.focus();
  }

  function hideOverlay() {
    clearInterval(lockTimer);
    if (overlay) overlay.remove();
    overlay = null;
  }

  function showBanner(text) {
    let b = document.querySelector('.remote-banner');
    if (!b) {
      b = document.createElement('div');
      b.className = 'remote-banner';
      document.body.appendChild(b);
    }
    b.textContent = text;
  }

  function hideBanner() {
    const b = document.querySelector('.remote-banner');
    if (b) b.remove();
  }

  const hubOnly = () => Promise.reject(new Error(HUB_ONLY));

  window.xr = {
    remote: true,
    init: async () => {
      if (!ws) connect();
      await whenReady();
      const data = await call('app:init');
      data.settings = { ...data.settings, ...loadLocal() };
      data.env = { ...data.env, remote: true, canControl, webCodecs: useWebCodecs() };
      knownStreams.clear();
      for (const s of data.streams) knownStreams.set(s.id, s);
      initialized = true;
      document.documentElement.classList.toggle('view-only', !canControl);
      return data;
    },
    refresh: () => call('devices:refresh'),
    verify: hubOnly,
    enableWireless: (id) => call('devices:enableWireless', id),
    disconnectWireless: (id) => call('devices:disconnectWireless', id),
    connect: (address) => call('devices:connect', address),
    scan: () => call('devices:scan'),
    forget: hubOnly,
    update: (id, patch) => call('devices:update', id, patch),
    startStream: (id, opts) => call('stream:start', id, opts),
    stopStream: (id) => call('stream:stop', id),
    startAll: () => call('stream:startAll'),
    stopAll: () => call('stream:stopAll'),
    // The hub's decoders don't matter here: report what this browser can play.
    setDecoderSupport: async (s) => {
      if (useWebCodecs() && s && s.h264) {
        support = s;
        sendJson({ t: 'support', support: s });
      }
      return null;
    },
    encoders: (id) => call('devices:encoders', id),
    // Only per-device view preferences can change from a remote browser.
    updateSettings: async (patch) => {
      const keys = Object.keys(patch || {});
      if (keys.some((k) => !LOCAL_SETTINGS.includes(k))) throw new Error(HUB_ONLY);
      const local = { ...loadLocal() };
      for (const k of keys) local[k] = patch[k];
      store.set(LOCAL_KEY, JSON.stringify(local));
      return local;
    },
    openExternal: async (url) => {
      if (!/^https?:\/\//.test(url)) throw new Error('Blocked URL');
      window.open(url, '_blank', 'noopener');
    },
    remoteInfo: hubOnly,
    remoteNewPin: hubOnly,
    remoteRevoke: hubOnly,
    setVisibleStreams: (ids) => {
      visible = Array.isArray(ids) ? ids.slice() : [];
      pushSubscriptions();
    },
    unpair: () => {
      sendJson({ t: 'unpair' });
      store.set(TOKEN_KEY, null);
      setTimeout(() => location.reload(), 300);
    },

    onDevices: on('devices'),
    onNotice: on('notice'),
    onStreamState: on('state'),
    onStreamLog: on('log'),
    onPacket: on('packet'),
    onRemoteStatus: () => () => {},
  };
})();
