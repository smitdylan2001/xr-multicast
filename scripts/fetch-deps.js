#!/usr/bin/env node
// Downloads build/runtime dependencies:
//   - scrcpy-server (the on-headset screen encoder, Apache 2.0), version pinned in
//     src/main/config.js. Always fetched; it is bundled with the app.
//   - Android platform-tools (adb), ONLY with --with-adb, for local development.
//     Release builds never bundle adb: the app offers to download it from Google
//     after the user accepts Google's terms (see src/main/adb-installer.js).
//
// Usage: node scripts/fetch-deps.js [--with-adb] [--platform linux|win32|darwin|all] [--force]

const fs = require('fs');
const path = require('path');
const os = require('os');
const { download: fetchFile } = require('../src/main/download');
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
  return { platforms, force: args.includes('--force'), withAdb: args.includes('--with-adb') };
}

function download(url, dest) {
  let shown = -1;
  return fetchFile(url, dest, ({ received, total }) => {
    const pct = total ? Math.floor((received / total) * 100) : -1;
    if (process.stdout.isTTY && pct !== shown) {
      shown = pct;
      process.stdout.write(`\r  ${path.basename(dest)}: ${pct}%   `);
    }
  }).then((r) => {
    if (process.stdout.isTTY) process.stdout.write('\n');
    return r;
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
  const { platforms, force, withAdb } = parseArgs();
  fs.mkdirSync(VENDOR, { recursive: true });
  await fetchScrcpyServer(force);
  if (withAdb) {
    console.log('By downloading Android SDK Platform-Tools you agree to https://developer.android.com/studio/terms');
    for (const p of platforms) await fetchPlatformTools(p, force);
  }
  console.log('Dependencies ready.');
})().catch((e) => {
  console.error(`\n✗ ${e.message}`);
  process.exit(1);
});
