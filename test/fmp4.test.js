const test = require('node:test');
const assert = require('node:assert');
const { Fmp4 } = require('../src/renderer/js/mse-decoder');

// Minimal exp-Golomb bit writer to build test SPS NAL units.
function bitWriter() {
  const bits = [];
  const w = {
    u(n, v) {
      for (let i = n - 1; i >= 0; i--) bits.push((v >> i) & 1);
      return w;
    },
    ue(v) {
      const x = v + 1;
      const len = Math.floor(Math.log2(x));
      for (let i = 0; i < len; i++) bits.push(0);
      return w.u(len + 1, x);
    },
    bytes() {
      bits.push(1); // rbsp stop bit
      while (bits.length % 8) bits.push(0);
      const out = [];
      for (let i = 0; i < bits.length; i += 8) out.push(parseInt(bits.slice(i, i + 8).join(''), 2));
      return Uint8Array.from(out);
    },
  };
  return w;
}

/** High-profile SPS for width x height (16-aligned macroblocks, cropped). */
function makeSps(width, height, profile = 100) {
  const mbW = Math.ceil(width / 16);
  const mbH = Math.ceil(height / 16);
  const w = bitWriter().u(8, 0x67).u(8, profile).u(8, 0).u(8, 51).ue(0);
  if (profile === 100) w.ue(1).ue(0).ue(0).u(1, 0).u(1, 0);
  w.ue(4).ue(2).ue(1).u(1, 0).ue(mbW - 1).ue(mbH - 1).u(1, 1).u(1, 1);
  const cropR = (mbW * 16 - width) / 2;
  const cropB = (mbH * 16 - height) / 2;
  if (cropR || cropB) w.u(1, 1).ue(0).ue(cropR).ue(0).ue(cropB);
  else w.u(1, 0);
  w.u(1, 0);
  return w.bytes();
}

const annexB = (...nals) => {
  const parts = [];
  for (const n of nals) parts.push(0, 0, 0, 1, ...n);
  return Uint8Array.from(parts);
};

/** Parse top-level boxes: [{type, size, body}] */
function boxes(buf, start = 0, end = buf.length) {
  const out = [];
  const dv = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  let o = start;
  while (o < end) {
    const size = dv.getUint32(o);
    const type = String.fromCharCode(...buf.subarray(o + 4, o + 8));
    out.push({ type, size, start: o, body: buf.subarray(o + 8, o + size) });
    assert.ok(size >= 8, `bad box size at ${o}`);
    o += size;
  }
  assert.strictEqual(o, end, 'boxes must exactly fill their parent');
  return out;
}

test('SPS parser reads dimensions, including cropping', () => {
  for (const [w, h] of [[1920, 1080], [1832, 1920], [3664, 1920], [1280, 720], [2064, 2208]]) {
    const info = Fmp4.parseSps(makeSps(w, h));
    assert.deepStrictEqual([info.width, info.height, info.profile, info.level], [w, h, 100, 51]);
  }
  const base = Fmp4.parseSps(makeSps(640, 480, 66));
  assert.deepStrictEqual([base.width, base.height, base.profile], [640, 480, 66]);
});

test('config packet -> codec string and avcC record', () => {
  const sps = makeSps(1920, 1080);
  const pps = Uint8Array.from([0x68, 0xee, 0x3c, 0x80]);
  const cfg = Fmp4.parseConfig(annexB(sps, pps));
  assert.strictEqual(cfg.codec, 'avc1.640033');
  const avcc = Fmp4.avcC(cfg);
  const [b] = boxes(avcc);
  assert.strictEqual(b.type, 'avcC');
  assert.deepStrictEqual([...b.body.subarray(0, 6)], [1, 0x64, 0x00, 0x33, 0xff, 0xe1]);
  assert.strictEqual((b.body[6] << 8) | b.body[7], sps.length);
  assert.strictEqual(b.body[8 + sps.length], 1); // one PPS
  assert.strictEqual(Fmp4.parseConfig(annexB(pps)), null);
});

test('init segment is a well-formed ftyp + moov with the stream size', () => {
  const cfg = Fmp4.parseConfig(annexB(makeSps(1832, 1920), Uint8Array.from([0x68, 0xee, 0x3c, 0x80])));
  const init = Fmp4.initSegment(cfg);
  const top = boxes(init);
  assert.deepStrictEqual(top.map((b) => b.type), ['ftyp', 'moov']);
  const moov = top[1];
  const kids = boxes(init, moov.start + 8, moov.start + moov.size);
  assert.deepStrictEqual(kids.map((b) => b.type), ['mvhd', 'trak', 'mvex']);
  const s = Buffer.from(init);
  for (const t of ['tkhd', 'mdhd', 'hdlr', 'stsd', 'avc1', 'avcC', 'trex']) assert.ok(s.includes(t), t);
  const avc1 = s.indexOf('avc1', s.indexOf('stsd'));
  // width/height live 24 bytes into the avc1 sample entry body
  assert.strictEqual(s.readUInt16BE(avc1 + 4 + 24), 1832);
  assert.strictEqual(s.readUInt16BE(avc1 + 4 + 26), 1920);
});

test('samples are AVCC length-prefixed without SPS/PPS/AUD', () => {
  const sps = makeSps(640, 480);
  const idr = Uint8Array.from([0x65, 0x88, 0x84, 0x00, 0x10]);
  const sample = Fmp4.toAvccSample(annexB(Uint8Array.from([0x09, 0xf0]), sps, Uint8Array.from([0x68, 0xee]), idr));
  assert.deepStrictEqual([...sample], [0, 0, 0, idr.length, ...idr]);
  // 3-byte start codes too
  const s2 = Fmp4.toAvccSample(Uint8Array.from([0, 0, 1, 0x41, 9, 8, 0, 0, 1, 0x41, 7]));
  assert.deepStrictEqual([...s2], [0, 0, 0, 3, 0x41, 9, 8, 0, 0, 0, 2, 0x41, 7]);
});

test('fragment: moof/mdat with correct data offset, tfdt and sample flags', () => {
  const sample = Uint8Array.from([0, 0, 0, 2, 0x65, 0xaa]);
  for (const key of [true, false]) {
    const frag = Fmp4.fragment(7, 2 ** 33 + 5, 1500, sample, key);
    const top = boxes(frag);
    assert.deepStrictEqual(top.map((b) => b.type), ['moof', 'mdat']);
    assert.deepStrictEqual([...top[1].body], [...sample]);
    const s = Buffer.from(frag);
    assert.strictEqual(s.readUInt32BE(s.indexOf('mfhd') + 8), 7);
    const tfdt = s.indexOf('tfdt');
    assert.strictEqual(s[tfdt + 4], 1); // version 1 (64-bit time)
    assert.strictEqual(s.readBigUInt64BE(tfdt + 8), BigInt(2 ** 33 + 5));
    const trun = s.indexOf('trun');
    assert.strictEqual(s.readUInt32BE(trun + 4) & 0xffffff, 0x000701);
    assert.strictEqual(s.readUInt32BE(trun + 8), 1);
    assert.strictEqual(s.readUInt32BE(trun + 12), top[1].start + 8); // data offset from moof start
    assert.strictEqual(s.readUInt32BE(trun + 16), 1500);
    assert.strictEqual(s.readUInt32BE(trun + 20), sample.length);
    assert.strictEqual(s.readUInt32BE(trun + 24), key ? 0x02000000 : 0x01010000);
  }
});
