const test = require('node:test');
const assert = require('node:assert');
const { StreamManager } = require('../src/main/stream-manager');
const { DEFAULT_SETTINGS } = require('../src/main/config');

function manager({ settings = {}, encoders, decoders }) {
  const store = { settings: { ...DEFAULT_SETTINGS, ...settings }, getHeadset: () => null, upsertHeadset() {} };
  const m = new StreamManager({}, {}, store, '/nonexistent');
  m.getEncoders = async () => {
    if (encoders instanceof Error) throw encoders;
    return encoders;
  };
  if (decoders) m.setDecoderSupport(decoders);
  return m;
}

const QUEST = [
  { codec: 'h264', name: 'c2.qti.avc.encoder', hw: true, alias: false },
  { codec: 'h265', name: 'c2.qti.hevc.encoder', hw: true, alias: false },
  { codec: 'av1', name: 'c2.android.av1.encoder', hw: false, alias: false },
];
const ALL_HW = { h264: { any: true, hw: true }, h265: { any: true, hw: true }, av1: { any: true, hw: true } };
const usb = { kind: 'usb', adbSerial: 'X' };
const entry = (extra = {}) => ({ serial: 'S1', ...extra });

test('auto picks H.265 when both sides have hardware support', async () => {
  const r = await manager({ encoders: QUEST, decoders: ALL_HW }).resolveCodec(entry(), usb);
  assert.deepStrictEqual(r, { codec: 'h265', encoder: 'c2.qti.hevc.encoder' });
});

test('auto skips software-only AV1 and falls to H.264 without H.265 decode', async () => {
  const r = await manager({ encoders: QUEST, decoders: { ...ALL_HW, h265: { any: false, hw: false } } }).resolveCodec(entry(), usb);
  assert.strictEqual(r.codec, 'h264');
});

test('auto uses H.264 before the renderer reports decoder support', async () => {
  const r = await manager({ encoders: QUEST }).resolveCodec(entry(), usb);
  assert.strictEqual(r.codec, 'h264');
});

test('explicit AV1 on a software encoder is allowed with a warning', async () => {
  const r = await manager({ settings: { usbCodec: 'av1' }, encoders: QUEST, decoders: ALL_HW }).resolveCodec(entry(), usb);
  assert.strictEqual(r.codec, 'av1');
  assert.match(r.note, /software encoder/);
});

test('explicit codec missing on the headset falls back to H.264', async () => {
  const r = await manager({ settings: { usbCodec: 'h265' }, encoders: QUEST.filter((e) => e.codec !== 'h265'), decoders: ALL_HW }).resolveCodec(entry(), usb);
  assert.strictEqual(r.codec, 'h264');
  assert.match(r.note, /no H\.265/);
});

test('a codec that failed earlier is not retried', async () => {
  const m = manager({ encoders: QUEST, decoders: ALL_HW });
  m.markCodecFailed('S1', 'h265');
  assert.strictEqual((await m.resolveCodec(entry(), usb)).codec, 'h264');
});

test('encoder enumeration failure means H.264', async () => {
  const r = await manager({ settings: { usbCodec: 'auto' }, encoders: new Error('old firmware'), decoders: ALL_HW }).resolveCodec(entry(), usb);
  assert.strictEqual(r.codec, 'h264');
});

const TEN = { ...ALL_HW, h265_10: { any: true, hw: true }, av1_10: { any: true, hw: true } };

test('10-bit is only used when enabled, for H.265/AV1, and decodable', () => {
  const off = manager({ encoders: QUEST, decoders: TEN });
  assert.strictEqual(off.useTenBit(entry(), 'h265'), false);
  const on = manager({ settings: { tenBit: true }, encoders: QUEST, decoders: TEN });
  assert.strictEqual(on.useTenBit(entry(), 'h265'), true);
  assert.strictEqual(on.useTenBit(entry(), 'av1'), true);
  assert.strictEqual(on.useTenBit(entry(), 'h264'), false);
  const noDecode = manager({ settings: { tenBit: true }, encoders: QUEST, decoders: { ...ALL_HW, h265_10: { any: false, hw: false } } });
  assert.strictEqual(noDecode.useTenBit(entry(), 'h265'), false);
});

test('10-bit is skipped after it failed or when forced to 8-bit', () => {
  const m = manager({ settings: { tenBit: true }, encoders: QUEST, decoders: TEN });
  assert.strictEqual(m.useTenBit(entry({ forceEightBit: true }), 'h265'), false);
  m.markTenBitFailed('S1', 'h265');
  assert.strictEqual(m.useTenBit(entry(), 'h265'), false);
  assert.strictEqual(m.useTenBit(entry(), 'av1'), true);
});

test('a remote viewer without 10-bit support disables it', () => {
  const m = manager({ settings: { tenBit: true }, encoders: QUEST, decoders: TEN });
  m.setViewerSupport('ipad', { h264: { any: true, hw: true }, h265: { any: true, hw: true } });
  assert.strictEqual(m.useTenBit(entry(), 'h265'), false);
  m.setViewerSupport('ipad', null);
  assert.strictEqual(m.useTenBit(entry(), 'h265'), true);
});

test('custom quality maps to encoder parameters', () => {
  const m = manager({ settings: { wifiQuality: 'custom', wifiCustom: { maxSize: 1600, bitRateMbps: 12.5, maxFps: 72 } }, encoders: QUEST });
  const q = m.qualityFor('wifi');
  assert.strictEqual(q.maxSize, 1600);
  assert.strictEqual(q.bitRate, 12_500_000);
  assert.strictEqual(q.maxFps, 72);
});
