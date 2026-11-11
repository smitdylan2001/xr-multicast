const test = require('node:test');
const assert = require('node:assert');
const fs = require('fs');
const path = require('path');
const vm = require('vm');

// decoder.js is a browser script; run it in a sandbox with a fake window.
const win = {};
vm.runInNewContext(fs.readFileSync(path.join(__dirname, '../src/renderer/js/decoder.js'), 'utf8'), { window: win, console });
const describe = win.describeVideoConfig;
const hex = (s) => Uint8Array.from(Buffer.from(s, 'hex'));

// Real SPS NAL units captured from encoders on the dev machine.
const SPS_MAIN10_X265 = '00000142010102200000030090000003000003007ba00398803c1cbb65ba4a4c2fff000100016a02020208000003000800000301e04000';
const SPS_10BIT_MISLABELLED_VAAPI = '00000142010121600000030090000003000003007ba003a0803c1c6eda2e490a569ffc0004000404000003000400000300f02000';
const SPS_MAIN8_VAAPI = '00000142010121600000030090000003000003007ba003a0803c1c6fa2e490a569ffc00040004040000003004000000f0200';

test('HEVC Main 10 stream gives a Main 10 codec string and 10-bit depth', () => {
  const d = describe('h265', hex(SPS_MAIN10_X265));
  assert.strictEqual(d.codec, 'hvc1.2.4.L123.90');
  assert.strictEqual(d.bitDepth, 10);
});

test('10-bit HEVC labelled as Main is corrected to Main 10', () => {
  const d = describe('h265', hex(SPS_10BIT_MISLABELLED_VAAPI));
  assert.strictEqual(d.bitDepth, 10);
  assert.match(d.codec, /^hvc1\.2\.4\.H123/);
});

test('8-bit HEVC Main stays Main', () => {
  const d = describe('h265', hex(SPS_MAIN8_VAAPI));
  assert.strictEqual(d.bitDepth, 8);
  assert.match(d.codec, /^hvc1\.1\.6\.H123/);
});

test('AV1 av1C record gives codec string, description and bit depth', () => {
  const rec = hex('81084c00' + '0a0b0000');
  const d = describe('av1', rec);
  assert.strictEqual(d.codec, 'av01.0.08M.10');
  assert.strictEqual(d.bitDepth, 10);
  assert.ok(d.description);
  assert.strictEqual(d.prepend, false);
});

test('H.264 codec string from SPS', () => {
  const d = describe('h264', hex('0000000167640033ac'));
  assert.strictEqual(d.codec, 'avc1.640033');
  assert.strictEqual(d.bitDepth, 8);
});
