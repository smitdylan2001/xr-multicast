const test = require('node:test');
const assert = require('node:assert');
const { detectGrid, resolveView } = require('../src/renderer/js/layout-detect');
const { BRANDS, detectBrand, detectVariant, presetCorrection, friendlyModel } = require('../src/shared/brands');

// Synthetic panel: `cols`×`rows` bright elliptical eye views on black, with some texture inside.
function frame(cols, rows, { w = 96, h = 96, level = 0.7, fill = 0.92 } = {}) {
  const luma = new Float32Array(w * h);
  const cw = w / cols;
  const ch = h / rows;
  for (let y = 0; y < h; y++) {
    for (let x = 0; x < w; x++) {
      const cx = (Math.floor(x / cw) + 0.5) * cw;
      const cy = (Math.floor(y / ch) + 0.5) * ch;
      const dx = (x - cx) / ((cw / 2) * fill);
      const dy = (y - cy) / ((ch / 2) * fill);
      const inside = dx * dx + dy * dy <= 1;
      luma[y * w + x] = inside ? level * (0.6 + 0.4 * Math.abs(Math.sin(x * 0.7 + y * 0.3))) : 0;
    }
  }
  return { luma, w, h };
}

const grid = (f) => {
  const g = detectGrid(f.luma, f.w, f.h);
  return g && `${g.cols}x${g.rows}`;
};

test('single view', () => assert.strictEqual(grid(frame(1, 1)), '1x1'));
test('side by side', () => assert.strictEqual(grid(frame(2, 1)), '2x1'));
test('top / bottom', () => assert.strictEqual(grid(frame(1, 2)), '1x2'));
test('2x2 grid', () => assert.strictEqual(grid(frame(2, 2)), '2x2'));
test('non-square sampling of a 2x2 grid', () => assert.strictEqual(grid(frame(2, 2, { w: 64, h: 80 })), '2x2'));
test('dim but visible content still detected', () => assert.strictEqual(grid(frame(2, 1, { level: 0.12 })), '2x1'));

test('black and near-black frames give no answer', () => {
  assert.strictEqual(detectGrid(new Float32Array(96 * 96), 96, 96), null);
  assert.strictEqual(grid(frame(2, 1, { level: 0.02 })), null);
});

test('a single view filling the frame is not split', () => {
  const w = 64;
  const luma = new Float32Array(w * w).fill(0.5);
  assert.strictEqual(detectGrid(luma, w, w).cols, 1);
});

test('a small off-centre view is not mistaken for two', () => {
  const w = 96;
  const luma = new Float32Array(w * w);
  for (let y = 30; y < 60; y++) for (let x = 5; x < 35; x++) luma[y * w + x] = 0.8;
  assert.strictEqual(grid({ luma, w, h: w }), '1x1');
});

test('resolveView: Quest-like wide frame is side by side', () => {
  const base = { layout: 'auto', rotation: 'auto', autoRotation: 0, frameW: 1832, frameH: 960 };
  assert.deepStrictEqual(resolveView({ ...base, grid: { cols: 2, rows: 1 } }), { layout: 'sbs', rotation: 0, rotationAuto: true, detected: true });
  // Faint gap between the eyes: the aspect ratio still says stereo.
  assert.strictEqual(resolveView({ ...base, grid: { cols: 1, rows: 1 } }).layout, 'sbs');
  assert.strictEqual(resolveView({ ...base, grid: null }).layout, 'sbs');
  // A dark horizon band must not turn a wide stereo frame into a grid or rotate it.
  assert.deepStrictEqual(resolveView({ ...base, grid: { cols: 2, rows: 2 } }).layout, 'sbs');
  const r = resolveView({ ...base, grid: { cols: 1, rows: 2 } });
  assert.deepStrictEqual([r.layout, r.rotation], ['sbs', 0]);
});

test('resolveView: PFD MR 2x2 grid with 90° default', () => {
  const r = resolveView({ layout: 'auto', rotation: 'auto', autoRotation: 90, frameW: 1784, frameH: 1920, grid: { cols: 2, rows: 2 } });
  assert.strictEqual(r.layout, 'quad');
  assert.strictEqual(r.rotation, 90);
});

test('resolveView: stacked stereo views get turned side by side', () => {
  // Raw frame has two views side by side; the 90° default stacks them, so auto adds another 90°.
  const r = resolveView({ layout: 'auto', rotation: 'auto', autoRotation: 90, frameW: 1784, frameH: 1920, grid: { cols: 2, rows: 1 } });
  assert.deepStrictEqual([r.layout, r.rotation], ['sbs', 180]);
  const q = resolveView({ layout: 'auto', rotation: 'auto', autoRotation: 0, frameW: 1000, frameH: 1000, grid: { cols: 1, rows: 2 } });
  assert.deepStrictEqual([q.layout, q.rotation], ['sbs', 90]);
});

test('resolveView: explicit settings win', () => {
  const r = resolveView({ layout: 'tb', rotation: 270, autoRotation: 90, frameW: 1000, frameH: 1000, grid: { cols: 1, rows: 2 } });
  assert.deepStrictEqual([r.layout, r.rotation, r.rotationAuto], ['tb', 270, false]);
  const m = resolveView({ layout: 'mono', rotation: 'auto', frameW: 1080, frameH: 2400, grid: { cols: 1, rows: 2 } });
  assert.deepStrictEqual([m.layout, m.rotation], ['mono', 0]);
});

test('resolveView: single-eye streams stay mono', () => {
  const r = resolveView({ layout: 'auto', rotation: 'auto', frameW: 916, frameH: 960, grid: { cols: 1, rows: 2 }, singleEye: true });
  assert.deepStrictEqual([r.layout, r.rotation], ['mono', 0]);
});

test('PFD MR is detected as a Play For Dream MR with its own preset', () => {
  const info = { manufacturer: 'Play For Dream', brand: 'YVR', model: 'PFDM MR', device: 'yvr_d3', product: 'yvr_d3', features: [] };
  const brand = detectBrand(info);
  assert.strictEqual(brand, 'yvr');
  assert.strictEqual(detectVariant(brand, info), 'pfd-mr');
  assert.strictEqual(`${BRANDS[brand].name} ${friendlyModel(brand, info)}`, 'Play For Dream MR');
  const c = presetCorrection(brand, 'pfd-mr');
  assert.strictEqual(c.autoRotation, 90);
  assert.strictEqual(c.rotation, 'auto');
  assert.strictEqual(c.layout, 'auto');
  assert.strictEqual(presetCorrection('meta', null).autoRotation, 0);
  assert.strictEqual(detectVariant('meta', { model: 'Quest 3', device: 'eureka' }), null);
});
