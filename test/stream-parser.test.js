const test = require('node:test');
const assert = require('node:assert');
const { ScrcpyVideoParser, ByteQueue, parseEncoderList } = require('../src/main/stream-session');
const { detectBrand, friendlyModel } = require('../src/shared/brands');

function deviceMeta(name) {
  const b = Buffer.alloc(64);
  b.write(name, 'utf8');
  return b;
}

function u32(n) {
  const b = Buffer.alloc(4);
  b.writeUInt32BE(n >>> 0);
  return b;
}

function session(w, h) {
  return Buffer.concat([u32(0x80000000), u32(w), u32(h)]);
}

function media(data, { config = false, key = false, pts = 0 } = {}) {
  const h = Buffer.alloc(12);
  let v = config ? 1n << 62n : BigInt(pts);
  if (key) v |= 1n << 61n;
  h.writeBigUInt64BE(v, 0);
  h.writeUInt32BE(data.length, 8);
  return Buffer.concat([h, data]);
}

function collect(stream, chunkSize) {
  const p = new ScrcpyVideoParser();
  const ev = { device: null, codec: null, sessions: [], packets: [], errors: [] };
  p.on('device', (d) => (ev.device = d));
  p.on('codec', (c) => (ev.codec = c));
  p.on('session', (s) => ev.sessions.push(s));
  p.on('packet', (pk) => ev.packets.push(pk));
  p.on('error', (e) => ev.errors.push(e));
  for (let i = 0; i < stream.length; i += chunkSize) p.push(stream.subarray(i, i + chunkSize));
  return ev;
}

const sps = Buffer.from([0, 0, 0, 1, 0x67, 0x64, 0x00, 0x33, 0xaa, 0, 0, 0, 1, 0x68, 0xce]);
const idr = Buffer.alloc(5000, 7);
const delta = Buffer.alloc(300, 3);

const stream = Buffer.concat([
  deviceMeta('Quest 3'),
  u32(0x68323634),
  session(3664, 1920),
  media(sps, { config: true }),
  media(idr, { key: true, pts: 16666 }),
  media(delta, { pts: 33333 }),
  session(1920, 1006),
  media(delta, { pts: 50000 }),
]);

for (const chunk of [1, 7, 64, 4096, stream.length]) {
  test(`parses scrcpy v5 video stream (chunk size ${chunk})`, () => {
    const ev = collect(stream, chunk);
    assert.strictEqual(ev.errors.length, 0);
    assert.strictEqual(ev.device, 'Quest 3');
    assert.strictEqual(ev.codec.name, 'h264');
    assert.deepStrictEqual(
      ev.sessions.map((s) => [s.width, s.height]),
      [
        [3664, 1920],
        [1920, 1006],
      ],
    );
    assert.strictEqual(ev.packets.length, 4);
    assert.deepStrictEqual(
      ev.packets.map((p) => [p.config, p.key, p.pts, p.data.length]),
      [
        [true, false, 0, sps.length],
        [false, true, 16666, idr.length],
        [false, false, 33333, delta.length],
        [false, false, 50000, delta.length],
      ],
    );
    assert.ok(ev.packets[1].data.every((b) => b === 7));
  });
}

test('reports encoder failure codec id', () => {
  const ev = collect(Buffer.concat([deviceMeta('x'), u32(1)]), 3);
  assert.strictEqual(ev.errors.length, 1);
});

test('ByteQueue takes across chunk boundaries', () => {
  const q = new ByteQueue();
  q.push(Buffer.from([1, 2]));
  q.push(Buffer.from([3, 4, 5]));
  assert.deepStrictEqual([...q.take(4)], [1, 2, 3, 4]);
  assert.strictEqual(q.length, 1);
  assert.deepStrictEqual([...q.take(1)], [5]);
});

test('parses scrcpy encoder list', () => {
  const out = `[server] INFO: Device: [Oculus] oculus Quest 3 (Android 14)
[server] INFO: List of video encoders:
    --video-codec=h264 --video-encoder=c2.qti.avc.encoder                (hw) [vendor]
    --video-codec=h264 --video-encoder=c2.android.avc.encoder            (sw)
    --video-codec=h265 --video-encoder=c2.qti.hevc.encoder               (hw) [vendor]
    --video-codec=h265 --video-encoder=OMX.qcom.video.encoder.hevc       (hw) [vendor] (alias for c2.qti.hevc.encoder)
    --video-codec=av1 --video-encoder=c2.android.av1.encoder             (sw)
    --video-codec=h264 --video-encoder=OMX.google.h264.encoder
[server] INFO: List of audio encoders:
    --audio-codec=opus --audio-encoder=c2.android.opus.encoder           (sw)`;
  const list = parseEncoderList(out);
  assert.deepStrictEqual(
    list.map((e) => [e.codec, e.name, e.hw, e.alias]),
    [
      ['h264', 'c2.qti.avc.encoder', true, false],
      ['h264', 'c2.android.avc.encoder', false, false],
      ['h265', 'c2.qti.hevc.encoder', true, false],
      ['h265', 'OMX.qcom.video.encoder.hevc', true, true],
      ['av1', 'c2.android.av1.encoder', false, false],
      ['h264', 'OMX.google.h264.encoder', false, false],
    ],
  );
});

test('detects headset brands', () => {
  const cases = [
    [{ manufacturer: 'Oculus', brand: 'oculus', model: 'Quest 3', device: 'eureka' }, 'meta', 'Quest 3'],
    [{ manufacturer: 'Oculus', brand: 'oculus', model: 'Quest', device: 'panther' }, 'meta', 'Quest 3S'],
    [{ manufacturer: 'Pico', brand: 'Pico', model: 'A8110', device: 'PICO 4' }, 'pico', 'PICO 4'],
    [{ manufacturer: 'YVR', brand: 'YVR', model: 'YVR 2', device: 'yvr2' }, 'yvr', 'YVR 2'],
    [{ manufacturer: 'Play For Dream', brand: 'PFDM', model: 'MR', device: 'mr' }, 'yvr', 'MR'],
    [{ manufacturer: 'HTC', brand: 'htc', model: 'VIVE Focus 3', device: 'kona' }, 'htc', 'VIVE Focus 3'],
    [{ manufacturer: 'samsung', brand: 'samsung', model: 'SM-I610', device: 'xrvst2' }, 'androidxr', 'Galaxy XR'],
    [{ manufacturer: 'Acme', brand: 'acme', model: 'X1', device: 'x1', features: ['android.software.xr.api.openxr'] }, 'androidxr', 'X1'],
    [{ manufacturer: 'Google', brand: 'google', model: 'Pixel 9', device: 'tokay' }, 'android', 'Pixel 9'],
  ];
  for (const [info, brand, model] of cases) {
    const b = detectBrand(info);
    assert.strictEqual(b, brand, JSON.stringify(info));
    assert.strictEqual(friendlyModel(b, info), model);
  }
});
