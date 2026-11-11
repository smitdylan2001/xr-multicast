const test = require('node:test');
const assert = require('node:assert');
const { neededLongSide, pickStep, stepIndex, scaledBitrate, decideResize, RES_STEPS } = require('../src/shared/view-size');

const QUEST3 = { w: 4128, h: 2208 }; // side-by-side stereo panel
const PFD = { w: 2856, h: 3072 }; // 2×2 grid, shown rotated 90°

test('corrected single eye in a 620×349 tile needs ~960 px on a Quest 3', () => {
  const need = neededLongSide(QUEST3, { tileW: 620, tileH: 349, mode: 'corrected', zoom: 1.45, outputAspect: '16:9', layout: 'sbs', rotation: 0 });
  assert.ok(need > 900 && need < 960, String(need));
  assert.strictEqual(pickStep(need, { cap: 2560, nativeLong: 4128 }), 960);
});

test('PFD MR quad layout rotated 90° needs a higher step (each eye is a quarter of the frame)', () => {
  const need = neededLongSide(PFD, { tileW: 620, tileH: 349, mode: 'corrected', zoom: 1.4, outputAspect: '16:9', layout: 'quad', rotation: 90 });
  assert.ok(need > 1000 && need < 1100, String(need));
  assert.strictEqual(pickStep(need, { cap: 2560, nativeLong: 3072 }), 1280);
});

test('raw view fits the whole frame into the tile', () => {
  assert.strictEqual(Math.round(neededLongSide(QUEST3, { tileW: 1000, tileH: 1000, mode: 'raw', layout: 'sbs' })), 1000);
  // Rotated: the long side becomes vertical, so the tile height limits it.
  assert.strictEqual(Math.round(neededLongSide(QUEST3, { tileW: 1000, tileH: 500, mode: 'raw', rotation: 90 })), 500);
});

test('stereo view needs fewer pixels per eye than single-eye corrected in the same tile', () => {
  const req = { tileW: 1200, tileH: 400, zoom: 1.4, outputAspect: '16:9', layout: 'sbs' };
  assert.ok(neededLongSide(QUEST3, { ...req, mode: 'stereo' }) <= neededLongSide(QUEST3, { ...req, mode: 'corrected' }));
});

test('hidden or empty tiles need nothing', () => {
  assert.strictEqual(neededLongSide(QUEST3, { visible: false, tileW: 800, tileH: 600, mode: 'raw' }), 0);
  assert.strictEqual(neededLongSide(QUEST3, { tileW: 0, tileH: 600, mode: 'raw' }), 0);
  assert.strictEqual(pickStep(0, { cap: 1920, nativeLong: 4128 }), RES_STEPS[0]);
});

test('between two steps the higher one is chosen, capped by preset and native size', () => {
  assert.strictEqual(pickStep(961, { cap: 2560, nativeLong: 4128 }), 1280);
  assert.strictEqual(pickStep(1280, { cap: 2560, nativeLong: 4128 }), 1280);
  assert.strictEqual(pickStep(3000, { cap: 1920, nativeLong: 4128 }), 1920);
  assert.strictEqual(pickStep(3000, { cap: 0, nativeLong: 3072 }), 3072, 'native is the last step');
  assert.strictEqual(pickStep(5000, { cap: 0, nativeLong: 4128 }), 4128);
});

test('step indexes include the native size', () => {
  assert.strictEqual(stepIndex(640, 3072), 0);
  assert.strictEqual(stepIndex(3072, 3072), RES_STEPS.filter((s) => s < 3072).length);
});

test('bitrate scales down with resolution but keeps a floor', () => {
  assert.strictEqual(scaledBitrate(8e6, 1920, 1920), 8e6);
  const half = scaledBitrate(8e6, 960, 1920);
  assert.ok(half > 2.5e6 && half < 3e6, String(half));
  assert.strictEqual(scaledBitrate(8e6, 640, 2560), 1.5e6);
  assert.strictEqual(scaledBitrate(1e6, 640, 2560), 1e6, 'never above the preset');
});

test('resize decisions: up immediately, down only after 2+ steps and a delay', () => {
  const n = 4128;
  assert.deepStrictEqual(decideResize({ current: 960, target: 1280, nativeLong: n, now: 0 }), { action: 'resize' });
  assert.deepStrictEqual(decideResize({ current: 1280, target: 960, nativeLong: n, now: 0 }), { action: 'none' }, 'one step lower is kept');
  assert.deepStrictEqual(decideResize({ current: 1920, target: 960, nativeLong: n, now: 0 }), { action: 'wait', waitMs: 5000 });
  assert.deepStrictEqual(decideResize({ current: 1920, target: 960, nativeLong: n, now: 3000, downSince: 0 }), { action: 'wait', waitMs: 2000 });
  assert.deepStrictEqual(decideResize({ current: 1920, target: 960, nativeLong: n, now: 5000, downSince: 0 }), { action: 'resize' });
  assert.deepStrictEqual(decideResize({ current: 1920, target: 640, nativeLong: n, now: 6000, downSince: 0, hiddenOnly: true }), { action: 'wait', waitMs: 9000 });
  assert.deepStrictEqual(decideResize({ current: 1280, target: 1280, nativeLong: n, now: 0 }), { action: 'none' });
});
