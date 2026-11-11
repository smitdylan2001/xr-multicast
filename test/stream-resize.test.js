const test = require('node:test');
const assert = require('node:assert');
const { StreamManager } = require('../src/main/stream-manager');
const { DEFAULT_SETTINGS } = require('../src/main/config');

function setup({ sizeLong = 1920, quality = 'high' } = {}) {
  const store = { settings: { ...DEFAULT_SETTINGS, usbQuality: quality }, getHeadset: () => null, upsertHeadset() {} };
  const m = new StreamManager({}, {}, store, '/nonexistent');
  m.panelSizes.set('USB1', { w: 4128, h: 2208 }); // Quest 3 panel
  const resized = [];
  m.resize = (entry, size) => resized.push(size);
  m.scheduleResize = () => {}; // evaluate synchronously in tests
  const entry = { serial: 'S1', adbSerial: 'USB1', transport: 'usb', state: 'live', gotMedia: true, liveSince: 0, autoSize: true, sizeLong, session: {} };
  m.entries.set('S1', entry);
  return { m, entry, resized };
}

const tile = (w, h, extra = {}) => ({ visible: true, tileW: w, tileH: h, mode: 'corrected', zoom: 1.45, outputAspect: '16:9', layout: 'sbs', rotation: 0, ...extra });

test('a bigger tile upgrades the stream immediately', () => {
  const { m, resized } = setup({ sizeLong: 960 });
  m.setViewSizes('local', { S1: tile(1800, 1000) });
  m.evaluateResize('S1');
  assert.deepStrictEqual(resized, [2560]);
});

test('a slightly different tile size does not restart the stream', () => {
  const { m, resized } = setup({ sizeLong: 960 });
  m.setViewSizes('local', { S1: tile(600, 338) });
  m.evaluateResize('S1');
  assert.deepStrictEqual(resized, []);
});

test('shrinking two or more steps waits before downgrading', () => {
  const { m, entry, resized } = setup({ sizeLong: 2560 });
  m.setViewSizes('local', { S1: tile(600, 338) });
  m.evaluateResize('S1');
  assert.deepStrictEqual(resized, [], 'not immediately');
  assert.ok(entry.downSince != null);
  entry.downSince -= 6000; // pretend 6 s passed
  m.evaluateResize('S1');
  assert.deepStrictEqual(resized, [960]);
});

test('the largest viewer wins (hub window hidden, tablet shows it big)', () => {
  const { m, resized } = setup({ sizeLong: 960 });
  m.setViewSizes('local', { S1: { visible: false, mode: 'corrected' } });
  m.setViewSizes('remote:ipad', { S1: tile(1600, 900) });
  m.evaluateResize('S1');
  assert.deepStrictEqual(resized, [2560]);
  // The tablet leaves: only the hidden hub tile remains.
  m.clearViewer('remote:ipad');
  assert.strictEqual(m.viewSizes.get('S1').size, 1);
});

test('never above the quality preset', () => {
  const { m, resized } = setup({ sizeLong: 960, quality: 'balanced' }); // 1920 cap
  m.setViewSizes('local', { S1: tile(3000, 1700) });
  m.evaluateResize('S1');
  assert.deepStrictEqual(resized, [1920]);
});

test('no resize while the stream is not live yet or just restarted', () => {
  const { m, entry, resized } = setup({ sizeLong: 960 });
  m.setViewSizes('local', { S1: tile(1800, 1000) });
  entry.state = 'starting';
  m.evaluateResize('S1');
  entry.state = 'live';
  entry.liveSince = Date.now();
  m.evaluateResize('S1');
  assert.deepStrictEqual(resized, []);
});

test('turning the setting off disables resizing', () => {
  const { m, entry, resized } = setup({ sizeLong: 960 });
  entry.autoSize = false;
  m.setViewSizes('local', { S1: tile(1800, 1000) });
  m.evaluateResize('S1');
  assert.deepStrictEqual(resized, []);
});
