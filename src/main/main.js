const { app, BrowserWindow, ipcMain, shell } = require('electron');
const fs = require('fs');
const path = require('path');
const { Adb } = require('./adb');
const { Store } = require('./store');
const { DeviceManager, HEADSET_COLORS } = require('./device-manager');
const { StreamManager } = require('./stream-manager');
const { RemoteServer } = require('./remote-server');
const { localIPv4s } = require('./discovery');
const { SCRCPY_VERSION, QUALITY_PRESETS, CODECS } = require('./config');
const { BRANDS } = require('../shared/brands');

let win = null;
let adb;
let store;
let devices;
let streams;
let remote;

function resolveServerPath() {
  const candidates = [
    process.env.XRMC_SCRCPY_SERVER,
    app.isPackaged ? path.join(process.resourcesPath, 'scrcpy-server') : null,
    path.join(__dirname, '..', '..', 'vendor', 'scrcpy-server'),
  ];
  return candidates.find((c) => c && fs.existsSync(c)) || candidates[candidates.length - 1];
}

function send(channel, ...args) {
  if (win && !win.isDestroyed()) win.webContents.send(channel, ...args);
}

function createWindow() {
  win = new BrowserWindow({
    width: 1440,
    height: 900,
    minWidth: 960,
    minHeight: 600,
    backgroundColor: '#0b0f17',
    title: 'XR MultiCast',
    autoHideMenuBar: true,
    icon: path.join(__dirname, '..', '..', 'build', 'icon.png'),
    webPreferences: {
      preload: path.join(__dirname, '..', 'preload.js'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
      backgroundThrottling: false,
    },
  });
  win.loadFile(path.join(__dirname, '..', 'renderer', 'index.html'));
  win.webContents.setWindowOpenHandler(({ url }) => {
    if (/^https?:\/\//.test(url)) shell.openExternal(url);
    return { action: 'deny' };
  });
  win.on('closed', () => (win = null));
}

// ------------------------------------------------------------ handler registry
//
// One table serves both the desktop window (IPC) and remote browsers (the
// WebSocket in remote-server.js). `access` decides what remote viewers may do:
//   view    - any paired browser
//   control - paired browsers when "allow remote control" is on
//   hub     - only the desktop window (USB verification, forgetting, settings)

const LOCAL = Object.freeze({ remote: false, canControl: true });
const handlers = new Map();

function handle(channel, access, fn) {
  handlers.set(channel, { access, fn });
}

async function dispatch(channel, args, ctx = LOCAL) {
  const h = handlers.get(channel);
  if (!h) throw new Error(`Unknown method "${channel}".`);
  if (ctx.remote) {
    if (h.access === 'hub') throw new Error('This can only be done on the hub computer.');
    if (h.access === 'control' && !ctx.canControl) throw new Error('View-only access: the hub has turned off remote control.');
    if (!Array.isArray(args) || args.length > 3) throw new Error('Bad arguments.');
  }
  return h.fn(ctx, ...args);
}

function wrap(channel) {
  return async (_evt, ...args) => {
    try {
      return { ok: true, value: await dispatch(channel, args, LOCAL) };
    } catch (e) {
      return { ok: false, error: e.message || String(e) };
    }
  };
}

// Remote input is untrusted: validate shapes before it reaches the managers.
const isPlainObject = (v) => !!v && typeof v === 'object' && !Array.isArray(v);

function checkId(id) {
  if (typeof id !== 'string' || !id || id.length > 128) throw new Error('Bad headset id.');
  return id;
}

function remoteStreamOpts(opts) {
  if (opts == null) return undefined;
  if (!isPlainObject(opts)) throw new Error('Bad options.');
  const out = {};
  if (['usb', 'wifi', 'auto'].includes(opts.transport)) out.transport = opts.transport;
  if (opts.restart) out.restart = true;
  if (typeof opts.codec === 'string' && CODECS[opts.codec]) out.codec = opts.codec;
  if (opts.decodeFailed) out.decodeFailed = true;
  return out;
}

function remotePatch(patch) {
  if (!isPlainObject(patch)) throw new Error('Bad update.');
  const out = {};
  if (typeof patch.name === 'string') out.name = patch.name.trim().slice(0, 40) || undefined;
  if (typeof patch.color === 'string' && /^#[0-9a-fA-F]{6}$/.test(patch.color)) out.color = patch.color;
  if (['auto', 'usb', 'wifi'].includes(patch.transport)) out.transport = patch.transport;
  if (typeof patch.viewMode === 'string' && /^[a-z]{1,20}$/.test(patch.viewMode)) out.viewMode = patch.viewMode;
  if (isPlainObject(patch.calibration)) {
    const cal = {};
    for (const [k, v] of Object.entries(patch.calibration).slice(0, 40)) {
      if (!/^[\w]{1,32}$/.test(k)) continue;
      if ((typeof v === 'number' && Number.isFinite(v)) || typeof v === 'boolean' || v === null) cal[k] = v;
      else if (typeof v === 'string' && v.length <= 40) cal[k] = v;
    }
    out.calibration = cal;
  }
  for (const k of Object.keys(out)) if (out[k] === undefined) delete out[k];
  return out;
}

function remoteSettingsPatch(patch) {
  if (!isPlainObject(patch)) throw new Error('Bad settings.');
  const out = { ...patch };
  if ('remoteEnabled' in out) out.remoteEnabled = !!out.remoteEnabled;
  if ('remoteAllowControl' in out) out.remoteAllowControl = !!out.remoteAllowControl;
  if ('remotePort' in out) {
    const p = Number(out.remotePort);
    if (!Number.isInteger(p) || p < 1024 || p > 65535) throw new Error('Remote access port must be between 1024 and 65535.');
    out.remotePort = p;
  }
  return out;
}

function registerHandlers() {
  handle('app:init', 'view', async (ctx) => ({
    brands: BRANDS,
    colors: HEADSET_COLORS,
    qualityPresets: QUALITY_PRESETS,
    codecs: CODECS,
    settings: store.settings,
    snapshot: devices.snapshot(),
    streams: streams.states(),
    env: {
      platform: process.platform,
      adbPath: ctx.remote ? null : adb.bin,
      adbVersion: await adb.version(),
      scrcpyVersion: SCRCPY_VERSION,
      serverAvailable: streams.serverAvailable(),
      localIps: localIPv4s().map((i) => i.address),
      appVersion: app.getVersion(),
      sandboxed: ctx.remote ? true : !app.commandLine.hasSwitch('no-sandbox'),
      remote: !!ctx.remote,
      canControl: !!ctx.canControl,
    },
  }));
  handle('devices:refresh', 'view', () => devices.poll());
  handle('devices:verify', 'hub', (_ctx, id, opts) => devices.verify(id, opts));
  handle('devices:enableWireless', 'control', (_ctx, id) => devices.enableWireless(checkId(id)));
  handle('devices:disconnectWireless', 'control', (_ctx, id) => devices.disconnectWireless(checkId(id)));
  handle('devices:connect', 'control', (ctx, address) => {
    if (ctx.remote && (typeof address !== 'string' || address.length > 100)) throw new Error('Bad address.');
    return devices.connectAddress(address);
  });
  handle('devices:scan', 'control', () => devices.scanNetwork());
  handle('devices:forget', 'hub', async (_ctx, id) => {
    await streams.stop(id);
    return devices.forget(id);
  });
  handle('devices:update', 'control', (ctx, id, patch) => devices.update(checkId(id), ctx.remote ? remotePatch(patch) : patch));
  handle('devices:encoders', 'control', (_ctx, id) => streams.encodersFor(checkId(id)));
  handle('stream:start', 'control', (ctx, id, opts) => streams.start(checkId(id), ctx.remote ? remoteStreamOpts(opts) : opts));
  handle('stream:stop', 'control', (_ctx, id) => streams.stop(checkId(id)));
  handle('stream:startAll', 'control', () => streams.startAll());
  handle('stream:stopAll', 'control', () => streams.stopAll());
  handle('stream:decoderSupport', 'hub', (_ctx, support) => streams.setDecoderSupport(support));
  handle('settings:update', 'hub', async (_ctx, patch) => {
    const before = store.settings;
    const remoteKeys = ['remoteEnabled', 'remotePort', 'remoteAllowControl'];
    const old = Object.fromEntries(remoteKeys.map((k) => [k, before[k]]));
    const s = store.updateSettings(remoteSettingsPatch(patch));
    devices.emitChange(true);
    if (remoteKeys.some((k) => s[k] !== old[k])) await remote.applySettings();
    return s;
  });
  handle('app:openExternal', 'hub', (_ctx, url) => {
    if (/^https?:\/\//.test(url)) return shell.openExternal(url);
    throw new Error('Blocked URL');
  });
  handle('remote:info', 'hub', () => remote.info());
  handle('remote:newPin', 'hub', () => remote.newPin());
  handle('remote:revoke', 'hub', (_ctx, id) => remote.revoke(id));

  for (const channel of handlers.keys()) ipcMain.handle(channel, wrap(channel));
}

// Linux desktops (GNOME on Wayland in particular) match windows to their
// launcher entry by this name, so the dock shows the right icon and title.
if (process.platform === 'linux') app.setDesktopName('xr-multicast.desktop');

if (!app.requestSingleInstanceLock()) {
  app.quit();
} else {
  app.on('second-instance', () => {
    if (win) {
      if (win.isMinimized()) win.restore();
      win.focus();
    }
  });

  app.whenReady().then(() => {
    const storeFile = path.join(app.getPath('userData'), 'xr-multicast.json');
    // Carry verified headsets and settings over from the app's previous name ("XR View").
    const legacyStore = path.join(app.getPath('appData'), 'XR View', 'xrview.json');
    if (!fs.existsSync(storeFile) && fs.existsSync(legacyStore)) {
      fs.mkdirSync(path.dirname(storeFile), { recursive: true });
      fs.copyFileSync(legacyStore, storeFile);
    }
    store = new Store(storeFile);
    adb = new Adb();
    devices = new DeviceManager(adb, store);
    streams = new StreamManager(adb, devices, store, resolveServerPath());
    remote = new RemoteServer({
      store,
      dispatch,
      streams,
      devices,
      rendererDir: path.join(__dirname, '..', 'renderer'),
      iconPath: path.join(__dirname, '..', '..', 'build', 'icon.png'),
      localIps: () => localIPv4s().map((i) => i.address),
    });

    devices.on('change', (snap) => send('devices:changed', snap));
    devices.on('notice', (n) => send('app:notice', n));
    devices.on('auto-cast', (id) => streams.start(id).catch(() => {}));
    streams.on('state', (s) => send('stream:state', s));
    streams.on('notice', (n) => send('app:notice', n));
    streams.on('packet', (id, p) => send('stream:packet', id, p.config, p.key, p.pts, p.data));
    streams.on('log', (id, line) => send('stream:log', id, line));
    remote.on('status', (s) => send('remote:status', s));
    remote.on('notice', (n) => send('app:notice', n));
    remote.on('log', (line) => console.warn(line));

    // Dev-only: lets test scripts reach the managers through the main-process inspector.
    if (!app.isPackaged && process.env.XRMC_DEV_HOOKS === '1') global.__xrmc = { store, devices, streams, remote, dispatch };

    registerHandlers();
    createWindow();
    devices.start();
    remote.applySettings().catch((e) => console.warn('remote server:', e.message));

    app.on('activate', () => {
      if (BrowserWindow.getAllWindows().length === 0) createWindow();
    });
  });

  let quitting = false;
  app.on('before-quit', (e) => {
    if (quitting || !streams) return;
    e.preventDefault();
    quitting = true;
    devices.stop();
    Promise.race([Promise.all([streams.stopAll(), remote.stop()]), new Promise((r) => setTimeout(r, 2500))]).finally(() => {
      store.flush();
      app.quit();
    });
  });

  app.on('window-all-closed', () => {
    if (process.platform !== 'darwin') app.quit();
  });
}
