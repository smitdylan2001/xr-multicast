#!/usr/bin/env node
// Regenerates the JS-parity fixtures for xrmc-media from the Electron app's
// reference implementation (src/renderer/js/decoder.js and mse-decoder.js):
//
//   node crates/xrmc-media/tests/fixtures/generate.js
//
// Output (next to this script):
//   h264-sps.txt  random valid H.264 SPS -> Fmp4.parseSps / describeConfig results
//   h265-sps.txt  random valid HEVC SPS  -> describeConfig results (+ generator size)
//   fmp4.txt      init segments, fragments and a full MseDecoder.push() session,
//                 recorded byte-for-byte from what the JS appends to its SourceBuffer
'use strict';

const fs = require('fs');
const path = require('path');
const vm = require('vm');

const ROOT = path.join(__dirname, '../../../..');
const { Fmp4, MseDecoder } = require(path.join(ROOT, 'src/renderer/js/mse-decoder.js'));
const win = {};
vm.runInNewContext(fs.readFileSync(path.join(ROOT, 'src/renderer/js/decoder.js'), 'utf8'), { window: win, console });
const describe = win.describeVideoConfig;

// Deterministic PRNG (mulberry32).
let seed = 0x5eed1234;
function rnd() {
  seed = (seed + 0x6d2b79f5) >>> 0;
  let t = seed;
  t = Math.imul(t ^ (t >>> 15), t | 1);
  t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
}
const int = (n) => Math.floor(rnd() * n);
const pick = (arr) => arr[int(arr.length)];
const hex = (b) => Buffer.from(b).toString('hex');

function bitWriter() {
  const bits = [];
  const w = {
    u(n, v) {
      for (let i = n - 1; i >= 0; i--) bits.push(Math.floor(v / 2 ** i) % 2);
      return w;
    },
    ue(v) {
      const x = v + 1;
      const len = Math.floor(Math.log2(x));
      for (let i = 0; i < len; i++) bits.push(0);
      return w.u(len + 1, x);
    },
    se(v) {
      return w.ue(v > 0 ? 2 * v - 1 : -2 * v);
    },
    rand(n) {
      for (let i = 0; i < n; i++) bits.push(int(2));
      return w;
    },
    bytes() {
      bits.push(1); // rbsp stop bit
      while (bits.length % 8) bits.push(0);
      const out = [];
      for (let i = 0; i < bits.length; i += 8) out.push(parseInt(bits.slice(i, i + 8).join(''), 2));
      return out;
    },
  };
  return w;
}

/** Insert emulation-prevention bytes into an RBSP (after the NAL header). */
function escape(rbsp, headerLen) {
  const out = rbsp.slice(0, headerLen);
  let zeros = 0;
  for (let i = headerLen; i < rbsp.length; i++) {
    const b = rbsp[i];
    if (zeros >= 2 && b <= 3) {
      out.push(3);
      zeros = 0;
    }
    out.push(b);
    zeros = b === 0 ? zeros + 1 : 0;
  }
  return out;
}

const annexB = (...nals) => {
  const parts = [];
  for (const n of nals) parts.push(0, 0, 0, 1, ...n);
  return Uint8Array.from(parts);
};

// ------------------------------------------------------------------ H.264

function randomH264Sps() {
  const profile = pick([66, 77, 88, 100, 100, 110, 122, 244]);
  const level = pick([30, 31, 40, 41, 42, 50, 51, 52]);
  const w = bitWriter().u(8, 0x67).u(8, profile).u(8, int(256) & 0xfc).u(8, level).ue(int(32));
  let chroma = 1;
  let depth = 8;
  if (profile >= 100) {
    chroma = profile === 244 ? pick([0, 1, 2, 3]) : profile === 122 ? pick([1, 2]) : 1;
    w.ue(chroma);
    if (chroma === 3) w.u(1, 0);
    depth = profile === 100 ? 8 : pick([8, 10]);
    w.ue(depth - 8).ue(depth - 8).u(1, 0);
    const scaling = int(3) === 0;
    w.u(1, scaling ? 1 : 0);
    if (scaling) {
      for (let i = 0; i < (chroma !== 3 ? 8 : 12); i++) {
        const present = int(2);
        w.u(1, present);
        if (!present) continue;
        const size = i < 6 ? 16 : 64;
        let last = 8;
        let next = 8;
        for (let j = 0; j < size; j++) {
          if (next !== 0) {
            const delta = int(8) === 0 ? -last : int(33) - 16; // sometimes end the list
            w.se(delta);
            next = (last + delta + 256) % 256;
          }
          last = next === 0 ? last : next;
        }
      }
    }
  }
  w.ue(int(13));
  const poc = pick([0, 1, 2]);
  w.ue(poc);
  if (poc === 0) w.ue(int(13));
  else if (poc === 1) {
    w.u(1, int(2)).se(int(41) - 20).se(int(41) - 20);
    const n = int(5);
    w.ue(n);
    for (let i = 0; i < n; i++) w.se(int(41) - 20);
  }
  w.ue(int(16)).u(1, int(2));
  const frameMbsOnly = int(4) === 0 ? 0 : 1;
  const mbW = 1 + int(240);
  const mapH = 1 + int(frameMbsOnly ? 135 : 68);
  w.ue(mbW - 1).ue(mapH - 1).u(1, frameMbsOnly);
  if (!frameMbsOnly) w.u(1, int(2));
  w.u(1, 1);
  const subW = chroma === 1 || chroma === 2 ? 2 : 1;
  const subH = chroma === 1 ? 2 : 1;
  const cropX = chroma === 0 ? 1 : subW;
  const cropY = (chroma === 0 ? 1 : subH) * (2 - frameMbsOnly);
  const crop = int(2);
  w.u(1, crop);
  if (crop) {
    const maxX = Math.floor((mbW * 16 - 1) / cropX / 2);
    const maxY = Math.floor(((2 - frameMbsOnly) * mapH * 16 - 1) / cropY / 2);
    for (const m of [maxX, maxX, maxY, maxY]) w.ue(int(Math.min(m, 8) + 1));
  }
  w.u(1, 0); // no VUI
  return { nal: escape(w.bytes(), 1), depth };
}

function h264Fixtures() {
  const lines = ['# hex(annexb config: SPS+PPS) codec width height profile level bit_depth'];
  for (let i = 0; i < 300; i++) {
    const { nal, depth } = randomH264Sps();
    const data = annexB(nal, [0x68, 0xce, 0x3c, 0x80]);
    const info = Fmp4.parseSps(Uint8Array.from(nal));
    const cfg = Fmp4.parseConfig(data);
    const d = describe('h264', data);
    if (cfg.codec !== d.codec) throw new Error('codec mismatch in the JS itself');
    lines.push([hex(data), d.codec, info.width, info.height, info.profile, info.level, depth].join(' '));
  }
  return lines.join('\n') + '\n';
}

// ------------------------------------------------------------------ H.265

function randomH265Sps() {
  const maxSub = pick([0, 0, 0, 1, 2, 3]);
  const space = int(6) === 0 ? 1 + int(3) : 0;
  const tier = int(2);
  const profile = pick([1, 1, 2, 2, 3, 4]);
  let compat = 0;
  for (let j = 0; j < 32; j++) if (j === profile || (j < 8 && int(3) === 0)) compat += 2 ** (31 - j);
  const cons = [];
  const nonZero = int(7);
  for (let i = 0; i < 6; i++) cons.push(i < nonZero ? int(256) : 0);
  const level = pick([30, 60, 90, 93, 120, 123, 150, 153, 156, 180, 186]);
  const w = bitWriter().u(8, 0x42).u(8, 0x01);
  w.u(4, int(16)).u(3, maxSub).u(1, int(2));
  w.u(2, space).u(1, tier).u(5, profile).u(32, compat);
  for (const c of cons) w.u(8, c);
  w.u(8, level);
  const sp = [];
  const sl = [];
  for (let i = 0; i < maxSub; i++) {
    sp.push(int(2));
    sl.push(int(2));
    w.u(1, sp[i]).u(1, sl[i]);
  }
  if (maxSub > 0) for (let i = maxSub; i < 8; i++) w.u(2, 0);
  for (let i = 0; i < maxSub; i++) {
    if (sp[i]) w.rand(88);
    if (sl[i]) w.u(8, int(256));
  }
  w.ue(int(16));
  const chroma = pick([0, 1, 1, 1, 2, 3]);
  w.ue(chroma);
  let separate = 0;
  if (chroma === 3) {
    separate = int(2);
    w.u(1, separate);
  }
  const picW = 8 * (1 + int(600));
  const picH = 8 * (1 + int(400));
  w.ue(picW).ue(picH);
  const [subW, subH] = chroma === 1 ? [2, 2] : chroma === 2 ? [2, 1] : [1, 1];
  const win = int(2);
  w.u(1, win);
  let width = picW;
  let height = picH;
  if (win) {
    const off = [int(4), int(4), int(4), int(4)];
    for (const o of off) w.ue(o);
    width -= subW * (off[0] + off[1]);
    height -= subH * (off[2] + off[3]);
  }
  const depth = pick([8, 8, 10, 10, 12]);
  w.ue(depth - 8).ue(depth - 8);
  w.rand(int(40));
  return { nal: escape(w.bytes(), 2), width, height, depth };
}

function h265Fixtures() {
  const vps = [0x40, 0x01, 0x0c, 0x01, 0xff, 0xff];
  const pps = [0x44, 0x01, 0xc1, 0x72, 0xb4, 0x62, 0x40];
  const lines = ['# hex(annexb config: VPS+SPS+PPS) codec bit_depth width height'];
  for (let i = 0; i < 300; i++) {
    const { nal, width, height, depth } = randomH265Sps();
    const data = annexB(vps, nal, pps);
    const d = describe('h265', data);
    if (d.bitDepth !== depth) throw new Error(`generator/JS disagree on bit depth: ${d.bitDepth} vs ${depth}`);
    lines.push([hex(data), d.codec, d.bitDepth, width, height].join(' '));
  }
  return lines.join('\n') + '\n';
}

// ------------------------------------------------------------------- fMP4

/** Run MseDecoder.push() with fake MSE/DOM objects and record every SourceBuffer append. */
function recordMseSession(packets) {
  const appended = [];
  const noop = () => {};
  const fakeVideo = () => ({
    style: {},
    setAttribute: noop,
    removeAttribute: noop,
    addEventListener: noop,
    load: noop,
    remove: noop,
    play: () => Promise.resolve(),
    paused: false,
    currentTime: 0,
    playbackRate: 1,
    readyState: 0,
  });
  class FakeMediaSource {
    constructor() {
      this.readyState = 'closed';
    }
    addEventListener(type, cb) {
      if (type === 'sourceopen') cb();
    }
    addSourceBuffer() {
      return {
        updating: false,
        mode: 'segments',
        buffered: { length: 0 },
        addEventListener: noop,
        appendBuffer: (data) => appended.push(Uint8Array.from(data)),
      };
    }
  }
  global.self = { MediaSource: FakeMediaSource };
  global.document = { createElement: fakeVideo, body: { appendChild: noop }, addEventListener: noop };
  URL.createObjectURL = () => 'blob:fake';
  URL.revokeObjectURL = noop;

  const errors = [];
  const dec = new MseDecoder({ onFrame: noop, onError: (e) => errors.push(e), family: 'h264' });
  const out = [];
  for (const p of packets) {
    const before = appended.length;
    dec.push(p.config, p.key, p.pts, p.data);
    out.push({ packet: p, segments: appended.slice(before) });
  }
  dec.close();
  delete global.self;
  delete global.document;
  if (errors.length) throw new Error(`MseDecoder errors: ${errors.map((e) => e.message)}`);
  return out;
}

function fmp4Fixtures() {
  const lines = [
    '# init <hex annexb config> <hex init segment>',
    '# fragment <seq> <base_time> <duration> <key 0|1> <hex sample> <hex fragment>',
    '# sample <hex annexb access unit> <hex avcc sample>',
    '# session <name>, then: push <config 0|1> <key 0|1> <pts> <hex data>, each followed by out <hex appended segment> lines',
  ];
  const pps = [0x68, 0xee, 0x3c, 0x80];
  const configs = [];
  for (let i = 0; i < 12; i++) configs.push(annexB(randomH264Sps().nal, pps));
  for (const c of configs) lines.push(`init ${hex(c)} ${hex(Fmp4.initSegment(Fmp4.parseConfig(c)))}`);

  for (let i = 0; i < 12; i++) {
    const sample = Array.from({ length: 1 + int(64) }, () => int(256));
    const seq = 1 + int(100000);
    const base = i === 0 ? 2 ** 33 + 5 : int(2 ** 40);
    const dur = 375 + int(4126);
    const key = int(2) === 1;
    lines.push(`fragment ${seq} ${base} ${dur} ${key ? 1 : 0} ${hex(sample)} ${hex(Fmp4.fragment(seq, base, dur, Uint8Array.from(sample), key))}`);
  }

  const aud = [0x09, 0xf0];
  const idr = (n) => [0x65, 0x88, ...Array.from({ length: n }, () => 1 + int(255))];
  const slice = (n) => [0x41, 0x9a, ...Array.from({ length: n }, () => 1 + int(255))];
  const samples = [
    annexB(aud, idr(10)),
    annexB(slice(5), slice(7)),
    Uint8Array.from([0, 0, 1, 0x41, 9, 8, 0, 0, 1, 0x41, 7]),
    Uint8Array.from([0, 0, 0, 1, 0x65, 1, 2, 0, 0, 0, 0, 1, 0x41, 3, 0, 0]),
    annexB(randomH264Sps().nal, pps),
    Uint8Array.from([]),
  ];
  for (const s of samples) lines.push(`sample ${hex(s) || '-'} ${hex(Fmp4.toAvccSample(s)) || '-'}`);

  const cfgA = annexB(randomH264Sps().nal, pps);
  const cfgB = annexB(randomH264Sps().nal, pps);
  const P = (config, key, pts, data) => ({ config, key, pts, data: Uint8Array.from(data) });
  const sessions = {
    'config-first': [
      P(false, false, 0, annexB(slice(4))), // before any config: ignored
      P(true, false, 0, cfgA),
      P(false, false, 5000, annexB(slice(4))), // waiting for a key frame: dropped
      P(false, true, 1000000, annexB(aud, idr(30))),
      P(false, false, 1016666, annexB(slice(8))),
      P(false, false, 1033333, annexB(aud, slice(8))),
      P(false, false, 1033333, annexB(slice(3))), // same pts
      P(false, false, 900000, annexB(slice(3))), // backwards
      P(false, false, 5000000, annexB(slice(3))), // long pause
      P(false, false, 5001000, annexB(slice(3))), // very short
      P(false, false, 5002000, annexB(aud)), // AUD only: empty sample
      P(true, false, 0, cfgA), // same config: no new init, wait for key
      P(false, false, 5010000, annexB(slice(3))),
      P(false, true, 5020000, annexB(idr(12))),
      P(false, false, 5040000, annexB(slice(2))),
      P(true, false, 0, cfgB), // new stream parameters: new init, timeline restarts
      P(false, true, 7000000, annexB(idr(20))),
      P(false, false, 7016000, annexB(slice(5))),
      P(true, false, 0, annexB(pps)), // config without SPS: ignored
      P(false, false, 7032000, annexB(slice(5))),
      P(false, true, 7048000, annexB(randomH264Sps().nal, pps, idr(9))),
    ],
    'in-band': [
      P(false, false, 0, annexB(slice(4))),
      P(false, true, 100, []), // empty key frame
      P(false, true, 200, Uint8Array.from([...cfgB, ...annexB(idr(15))])),
      P(false, false, 16866, annexB(slice(6))),
      P(false, false, 33533, annexB(slice(6))),
    ],
  };
  for (const [name, packets] of Object.entries(sessions)) {
    lines.push(`session ${name}`);
    for (const { packet, segments } of recordMseSession(packets)) {
      lines.push(`push ${packet.config ? 1 : 0} ${packet.key ? 1 : 0} ${packet.pts} ${hex(packet.data) || '-'}`);
      for (const s of segments) lines.push(`out ${hex(s)}`);
    }
  }
  return lines.join('\n') + '\n';
}

fs.writeFileSync(path.join(__dirname, 'h264-sps.txt'), h264Fixtures());
fs.writeFileSync(path.join(__dirname, 'h265-sps.txt'), h265Fixtures());
fs.writeFileSync(path.join(__dirname, 'fmp4.txt'), fmp4Fixtures());
console.log('fixtures written to', __dirname);
