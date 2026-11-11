const test = require('node:test');
const assert = require('node:assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { Adb, resolveAdbPath } = require('../src/main/adb');
const { managedAdbPath } = require('../src/main/adb-installer');
const { download } = require('../src/main/download');

const EXE = process.platform === 'win32' ? 'adb.exe' : 'adb';

function withEnv(vars, fn) {
  const saved = {};
  for (const k of Object.keys(vars)) {
    saved[k] = process.env[k];
    if (vars[k] === undefined) delete process.env[k];
    else process.env[k] = vars[k];
  }
  try {
    return fn();
  } finally {
    for (const k of Object.keys(saved)) {
      if (saved[k] === undefined) delete process.env[k];
      else process.env[k] = saved[k];
    }
  }
}

test('an explicit XRMC_ADB override is used exclusively', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'xrmc-adb-'));
  const fake = path.join(dir, EXE);
  fs.writeFileSync(fake, '');
  withEnv({ XRMC_ADB: fake, XRVIEW_ADB: undefined }, () => assert.strictEqual(resolveAdbPath(), fake));
  withEnv({ XRMC_ADB: path.join(dir, 'missing'), XRVIEW_ADB: undefined }, () => assert.strictEqual(resolveAdbPath(), null));
  fs.rmSync(dir, { recursive: true, force: true });
});

test('the copy installed by the app is preferred over other adbs', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'xrmc-managed-'));
  fs.mkdirSync(path.join(root, 'platform-tools'));
  fs.writeFileSync(managedAdbPath(root), '');
  withEnv({ XRMC_ADB: undefined, XRVIEW_ADB: undefined }, () => assert.strictEqual(resolveAdbPath(root), managedAdbPath(root)));
  fs.rmSync(root, { recursive: true, force: true });
});

test('without adb, commands fail with an actionable message instead of crashing', async () => {
  const adb = new Adb(null);
  assert.strictEqual(adb.bin, null);
  await assert.rejects(adb.run(['devices']), /not installed/);
  assert.strictEqual(await adb.version(), null);
  assert.throws(() => adb.spawn(['shell']), /not installed/);
});

test('downloads refuse plain HTTP', async () => {
  await assert.rejects(download('http://dl.google.com/x.zip', path.join(os.tmpdir(), 'x.zip')), /non-HTTPS/);
});
