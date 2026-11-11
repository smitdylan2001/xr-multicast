const { contextBridge, ipcRenderer } = require('electron');

async function call(channel, ...args) {
  const res = await ipcRenderer.invoke(channel, ...args);
  if (!res.ok) throw new Error(res.error);
  return res.value;
}

function subscribe(channel, cb) {
  const listener = (_e, ...args) => cb(...args);
  ipcRenderer.on(channel, listener);
  return () => ipcRenderer.removeListener(channel, listener);
}

contextBridge.exposeInMainWorld('xr', {
  init: () => call('app:init'),
  refresh: () => call('devices:refresh'),
  verify: (id, opts) => call('devices:verify', id, opts),
  enableWireless: (id) => call('devices:enableWireless', id),
  disconnectWireless: (id) => call('devices:disconnectWireless', id),
  connect: (address) => call('devices:connect', address),
  scan: () => call('devices:scan'),
  forget: (id) => call('devices:forget', id),
  update: (id, patch) => call('devices:update', id, patch),
  startStream: (id, opts) => call('stream:start', id, opts),
  stopStream: (id) => call('stream:stop', id),
  startAll: () => call('stream:startAll'),
  stopAll: () => call('stream:stopAll'),
  setDecoderSupport: (support) => call('stream:decoderSupport', support),
  encoders: (id) => call('devices:encoders', id),
  updateSettings: (patch) => call('settings:update', patch),
  openExternal: (url) => call('app:openExternal', url),
  adbInstall: (opts) => call('adb:install', opts),
  adbRecheck: () => call('adb:recheck'),
  onAdbProgress: (cb) => subscribe('adb:progress', cb),
  remoteInfo: () => call('remote:info'),
  remoteNewPin: () => call('remote:newPin'),
  remoteRevoke: (id) => call('remote:revoke', id),

  onDevices: (cb) => subscribe('devices:changed', cb),
  onNotice: (cb) => subscribe('app:notice', cb),
  onStreamState: (cb) => subscribe('stream:state', cb),
  onStreamLog: (cb) => subscribe('stream:log', cb),
  onPacket: (cb) => subscribe('stream:packet', cb),
  onRemoteStatus: (cb) => subscribe('remote:status', cb),
});
