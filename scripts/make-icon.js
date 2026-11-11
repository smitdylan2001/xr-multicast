#!/usr/bin/env node
// Renders the app icon (build/icon.png, 512x512) and writes build/icon.svg.
// Pure Node (zlib only): signed-distance shapes with 4x4 supersampling.

const fs = require('fs');
const path = require('path');
const zlib = require('zlib');

const SIZE = 512;
const OUT = path.join(__dirname, '..', 'build');

const hex = (h) => [1, 3, 5].map((i) => parseInt(h.slice(i, i + 2), 16));
const BG_TOP = hex('#1d2b4d');
const BG_BOTTOM = hex('#0b0f17');
const ACCENT = hex('#4f8cff');
const EYE = hex('#8db4ff');

function sdRoundBox(px, py, cx, cy, hw, hh, r) {
  const qx = Math.abs(px - cx) - hw + r;
  const qy = Math.abs(py - cy) - hh + r;
  return Math.hypot(Math.max(qx, 0), Math.max(qy, 0)) + Math.min(Math.max(qx, qy), 0) - r;
}

function sample(x, y) {
  // returns [r, g, b, a]
  const bg = sdRoundBox(x, y, 256, 256, 240, 240, 108);
  if (bg > 0) return [0, 0, 0, 0];
  const t = y / SIZE;
  let c = BG_TOP.map((v, i) => v + (BG_BOTTOM[i] - v) * t);
  const visor = Math.abs(sdRoundBox(x, y, 256, 272, 186, 104, 72)) - 14;
  const strap = Math.abs(sdRoundBox(x, y, 256, 156, 120, 40, 40)) - 10;
  const strapMask = y < 168 ? 1 : 0;
  const eyeL = Math.hypot(x - 182, y - 272) - 44;
  const eyeR = Math.hypot(x - 330, y - 272) - 44;
  if ((strap < 0 && strapMask) || visor < 0) c = ACCENT;
  if (eyeL < 0 || eyeR < 0) c = EYE;
  return [c[0], c[1], c[2], 255];
}

function render() {
  const S = 4;
  const raw = Buffer.alloc((SIZE * 4 + 1) * SIZE);
  for (let y = 0; y < SIZE; y++) {
    raw[y * (SIZE * 4 + 1)] = 0; // filter: none
    for (let x = 0; x < SIZE; x++) {
      let r = 0, g = 0, b = 0, a = 0;
      for (let sy = 0; sy < S; sy++) {
        for (let sx = 0; sx < S; sx++) {
          const p = sample(x + (sx + 0.5) / S, y + (sy + 0.5) / S);
          r += p[0] * p[3];
          g += p[1] * p[3];
          b += p[2] * p[3];
          a += p[3];
        }
      }
      const o = y * (SIZE * 4 + 1) + 1 + x * 4;
      raw[o] = a ? Math.round(r / a) : 0;
      raw[o + 1] = a ? Math.round(g / a) : 0;
      raw[o + 2] = a ? Math.round(b / a) : 0;
      raw[o + 3] = Math.round(a / (S * S));
    }
  }
  return raw;
}

const CRC_TABLE = Array.from({ length: 256 }, (_, n) => {
  let c = n;
  for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
  return c >>> 0;
});
function crc32(buf) {
  let c = 0xffffffff;
  for (const b of buf) c = CRC_TABLE[(c ^ b) & 255] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}
function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const td = Buffer.concat([Buffer.from(type), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(td));
  return Buffer.concat([len, td, crc]);
}

function png(raw) {
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(SIZE, 0);
  ihdr.writeUInt32BE(SIZE, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // RGBA
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk('IHDR', ihdr),
    chunk('IDAT', zlib.deflateSync(raw, { level: 9 })),
    chunk('IEND', Buffer.alloc(0)),
  ]);
}

const SVG = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512">
  <defs><linearGradient id="g" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#1d2b4d"/><stop offset="1" stop-color="#0b0f17"/></linearGradient></defs>
  <rect x="16" y="16" width="480" height="480" rx="108" fill="url(#g)"/>
  <path d="M136 168 V156 a40 40 0 0 1 40 -40 H336 a40 40 0 0 1 40 40 V168" fill="none" stroke="#4f8cff" stroke-width="20"/>
  <rect x="70" y="168" width="372" height="208" rx="72" fill="none" stroke="#4f8cff" stroke-width="28"/>
  <circle cx="182" cy="272" r="44" fill="#8db4ff"/>
  <circle cx="330" cy="272" r="44" fill="#8db4ff"/>
</svg>
`;

fs.mkdirSync(OUT, { recursive: true });
fs.writeFileSync(path.join(OUT, 'icon.png'), png(render()));
fs.writeFileSync(path.join(OUT, 'icon.svg'), SVG);
console.log('Wrote build/icon.png and build/icon.svg');
