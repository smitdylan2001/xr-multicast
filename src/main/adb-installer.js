// Installs Google's Android SDK Platform-Tools (adb) into the app's own data
// folder, on the user's request and after they accept Google's terms. adb is not
// shipped with XR MultiCast, so the app itself stays MIT-licensed.

const fs = require('fs');
const os = require('os');
const path = require('path');
const { download } = require('./download');
const { PLATFORM_TOOLS_URLS } = require('./config');

const ADB_EXE = process.platform === 'win32' ? 'adb.exe' : 'adb';

/** Path of adb inside a managed platform-tools folder. */
function managedAdbPath(root) {
  return path.join(root, 'platform-tools', ADB_EXE);
}

/**
 * @param {string} root folder to install into (its `platform-tools/` subfolder is replaced)
 * @param {(p: {received: number, total: number}) => void} [onProgress]
 * @returns {Promise<string>} path to the installed adb
 */
async function installPlatformTools(root, onProgress) {
  const url = PLATFORM_TOOLS_URLS[process.platform];
  if (!url) throw new Error(`No platform-tools download for ${process.platform}.`);
  const extract = require('extract-zip');
  const zip = path.join(os.tmpdir(), `xrmc-platform-tools-${process.pid}-${Date.now()}.zip`);
  const staging = `${root}.new`;
  try {
    await download(url, zip, onProgress);
    fs.rmSync(staging, { recursive: true, force: true });
    fs.mkdirSync(staging, { recursive: true });
    await extract(zip, { dir: path.resolve(staging) });
    const adb = managedAdbPath(staging);
    if (!fs.existsSync(adb)) throw new Error('The downloaded package does not contain adb.');
    if (process.platform !== 'win32') {
      for (const f of fs.readdirSync(path.join(staging, 'platform-tools'))) {
        const p = path.join(staging, 'platform-tools', f);
        if (fs.statSync(p).isFile()) fs.chmodSync(p, 0o755);
      }
    }
    fs.rmSync(root, { recursive: true, force: true });
    fs.renameSync(staging, root);
    return managedAdbPath(root);
  } finally {
    fs.rmSync(zip, { force: true });
    fs.rmSync(`${zip}.part`, { force: true });
    fs.rmSync(staging, { recursive: true, force: true });
  }
}

module.exports = { installPlatformTools, managedAdbPath, ADB_EXE };
