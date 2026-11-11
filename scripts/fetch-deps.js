#!/usr/bin/env node
// Downloads the runtime dependencies that are bundled with the app:
//   - Android platform-tools (adb) for the requested platform(s)
//   - scrcpy-server (the on-headset screen encoder), version pinned in src/main/config.js
//
// Usage: node scripts/fetch-deps.js [--platform linux|win32|darwin|all] [--force]

const fs = require('fs');
const path = require('path');
const os = require('os');
const https = require('https');
const { SCRCPY_SERVER_URL, SCRCPY_VERSION, PLATFORM_TOOLS_URLS } = require('../src/main/config');

const ROOT = path.join(__dirname, '..');
const VENDOR = path.join(ROOT, 'vendor');

function parseArgs() {
  const args = process.argv.slice(2);
  let platform = process.platform;
  const i = args.indexOf('--platform');
  if (i !== -1 && args[i + 1]) platform = args[i + 1];
  const platforms = platform === 'all' ? Object.keys(PLATFORM_TOOLS_URLS) : [platform];
  for (const p of platforms) {
    if (!PLATFORM_TOOLS_URLS[p]) throw new Error(`Unsupported platform: ${p}`);
  }
  return { platforms, force: args.includes('--force') };
}

function download(url, dest, redirects = 0) {
  return new Promise((resolve, reject) => {
    if (redirects > 10) return reject(new Error('Too many redirects'));
    https
      .get(url, { headers: { 'User-Agent': 'xr-multicast' } }, (res) => {
        if ([301, 302, 303, 307, 308].includes(res.statusCode) && res.headers.location) {
          res.resume();
          const next = new URL(res.headers.location, url).toString();
          return resolve(download(next, dest, redirects + 1));
        }
        if (res.statusCode !== 200) {
          res.resume();
          return reject(new Error(`GET ${url} -> HTTP ${res.statusCode}`));
        }
        const total = Number(res.headers['content-length']) || 0;
        let received = 0;
        const tmp = `${dest}.part`;
        const out = fs.createWriteStream(tmp);
        res.on('data', (c) => {
          received += c.length;
          if (total && process.stdout.isTTY) {
            process.stdout.write(`\r  ${path.basename(dest)}: ${Math.round((received / total) * 100)}%   `);
          }
        });
        res.pipe(out);
        out.on('finish', () => {
          out.close(() => {
            fs.renameSync(tmp, dest);
            if (process.stdout.isTTY) process.stdout.write('\n');
            resolve(dest);
          });
        });
        out.on('error', reject);
      })
      .on('error', reject);
  });
}

async function fetchScrcpyServer(force) {
  const dest = path.join(VENDOR, 'scrcpy-server');
  const versionFile = path.join(VENDOR, 'scrcpy-server.version');
  const current = fs.existsSync(versionFile) ? fs.readFileSync(versionFile, 'utf8').trim() : null;
  if (!force && fs.existsSync(dest) && current === SCRCPY_VERSION) {
    console.log(`✓ scrcpy-server v${SCRCPY_VERSION} already present`);
    return;
  }
  console.log(`↓ scrcpy-server v${SCRCPY_VERSION}`);
  await download(SCRCPY_SERVER_URL, dest);
  fs.writeFileSync(versionFile, SCRCPY_VERSION);
}

async function fetchPlatformTools(platform, force) {
  const extract = require('extract-zip');
  const targetDir = path.join(VENDOR, 'platform-tools', platform);
  const adbName = platform === 'win32' ? 'adb.exe' : 'adb';
  const adbPath = path.join(targetDir, 'platform-tools', adbName);
  if (!force && fs.existsSync(adbPath)) {
    console.log(`✓ platform-tools (${platform}) already present`);
    return;
  }
  console.log(`↓ platform-tools (${platform})`);
  const zipPath = path.join(os.tmpdir(), `platform-tools-${platform}-${Date.now()}.zip`);
  await download(PLATFORM_TOOLS_URLS[platform], zipPath);
  fs.rmSync(targetDir, { recursive: true, force: true });
  fs.mkdirSync(targetDir, { recursive: true });
  await extract(zipPath, { dir: path.resolve(targetDir) });
  fs.rmSync(zipPath, { force: true });
  if (platform !== 'win32') {
    for (const f of fs.readdirSync(path.join(targetDir, 'platform-tools'))) {
      const p = path.join(targetDir, 'platform-tools', f);
      if (fs.statSync(p).isFile()) fs.chmodSync(p, 0o755);
    }
  }
}

(async () => {
  const { platforms, force } = parseArgs();
  fs.mkdirSync(VENDOR, { recursive: true });
  await fetchScrcpyServer(force);
  for (const p of platforms) await fetchPlatformTools(p, force);
  console.log('Dependencies ready.');
})().catch((e) => {
  console.error(`\n✗ ${e.message}`);
  process.exit(1);
});
