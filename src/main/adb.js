const { execFile, spawn } = require('child_process');
const fs = require('fs');
const path = require('path');

function resolveAdbPath() {
  const exe = process.platform === 'win32' ? 'adb.exe' : 'adb';
  const candidates = [];
  if (process.env.XRMC_ADB) candidates.push(process.env.XRMC_ADB);
  if (process.env.XRVIEW_ADB) candidates.push(process.env.XRVIEW_ADB); // previous app name
  if (process.resourcesPath) candidates.push(path.join(process.resourcesPath, 'platform-tools', exe));
  candidates.push(path.join(__dirname, '..', '..', 'vendor', 'platform-tools', process.platform, 'platform-tools', exe));
  for (const c of candidates) {
    if (c && fs.existsSync(c)) return c;
  }
  return exe; // fall back to whatever adb is on PATH
}

/** Is this adb serial a network transport (tcpip or mDNS) rather than USB? */
function isNetworkSerial(serial) {
  return /:\d+$/.test(serial) || serial.includes('._adb-tls-connect.') || serial.includes('._adb._tcp');
}

class Adb {
  constructor(bin = resolveAdbPath()) {
    this.bin = bin;
  }

  run(args, { serial, timeout = 15000, maxBuffer = 8 * 1024 * 1024 } = {}) {
    const full = serial ? ['-s', serial, ...args] : args;
    return new Promise((resolve, reject) => {
      execFile(this.bin, full, { timeout, maxBuffer, windowsHide: true }, (err, stdout, stderr) => {
        if (err) {
          const msg = (stderr || stdout || err.message || '').toString().trim();
          const e = new Error(msg || `adb ${full.join(' ')} failed`);
          e.code = err.code;
          return reject(e);
        }
        resolve(stdout.toString());
      });
    });
  }

  shell(serial, command, opts) {
    return this.run(['shell', command], { serial, ...opts });
  }

  spawn(args, serial) {
    const full = serial ? ['-s', serial, ...args] : args;
    return spawn(this.bin, full, { windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] });
  }

  async version() {
    try {
      const out = await this.run(['version'], { timeout: 5000 });
      const m = out.match(/^Version\s+([\w.\-]+)/m) || out.match(/version\s+([\d.]+)/i);
      return m ? m[1] : out.split('\n')[0].trim();
    } catch {
      return null;
    }
  }

  startServer() {
    return this.run(['start-server'], { timeout: 20000 });
  }

  /** @returns {Promise<Array<{serial:string,state:string,network:boolean,props:Record<string,string>}>>} */
  async devices() {
    const out = await this.run(['devices', '-l'], { timeout: 8000 });
    const list = [];
    for (const raw of out.split(/\r?\n/)) {
      const line = raw.trim();
      if (!line || line.startsWith('List of devices') || line.startsWith('*')) continue;
      const parts = line.split(/\s+/);
      const [serial, state, ...rest] = parts;
      if (!serial || !state || serial.startsWith('emulator-')) continue;
      const props = {};
      for (const p of rest) {
        const i = p.indexOf(':');
        if (i > 0) props[p.slice(0, i)] = p.slice(i + 1);
      }
      list.push({ serial, state, network: isNetworkSerial(serial), props });
    }
    return list;
  }

  async connect(address, timeout = 8000) {
    const out = await this.run(['connect', address], { timeout });
    if (/connected to|already connected/i.test(out)) return out.trim();
    throw new Error(out.trim() || `Could not connect to ${address}`);
  }

  async disconnect(address) {
    try {
      await this.run(['disconnect', address], { timeout: 5000 });
    } catch {
      /* already gone */
    }
  }

  async mdnsServices() {
    try {
      const out = await this.run(['mdns', 'services'], { timeout: 5000 });
      const found = [];
      for (const line of out.split(/\r?\n/)) {
        const m = line.match(/^(\S+)\s+(_adb[\w-]*\._tcp)\.?\s+([\d.]+):(\d+)/);
        if (m) found.push({ name: m[1], service: m[2], ip: m[3], port: Number(m[4]) });
      }
      return found;
    } catch {
      return [];
    }
  }
}

module.exports = { Adb, resolveAdbPath, isNetworkSerial };
