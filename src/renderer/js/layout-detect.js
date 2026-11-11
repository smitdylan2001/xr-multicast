'use strict';

// Panel layout detection and resolution, shared by the renderer (browser) and
// the unit tests (Node).
//
// Standalone headsets mirror their panel image: one or more barrel-distorted
// eye views, each a bright region surrounded by black (outside the lens-visible
// area). Counting those regions along each axis tells us whether the frame is a
// single view, two views side by side / stacked, or a 2×2 grid.

(function (root, factory) {
  const api = factory();
  if (typeof module === 'object' && module.exports) module.exports = api;
  if (root) Object.assign(root, { LayoutDetect: api });
})(typeof window !== 'undefined' ? window : null, function () {
  const MIN_MEAN = 0.03; // frames darker than this (0..1) carry no layout information
  const DIP_RATIO = 0.6; // centre dip must be below this fraction of the weaker peak
  const MIN_PEAK = 0.4; // each peak must reach this fraction of the profile maximum

  function smooth(p, radius) {
    const out = new Float64Array(p.length);
    for (let i = 0; i < p.length; i++) {
      let s = 0;
      let n = 0;
      for (let j = Math.max(0, i - radius); j <= Math.min(p.length - 1, i + radius); j++) {
        s += p[j];
        n++;
      }
      out[i] = s / n;
    }
    return out;
  }

  /** Does a 1-D brightness profile show two bright lobes split by a dark gap near the centre? */
  function splitProfile(profile) {
    const n = profile.length;
    const p = smooth(profile, Math.max(1, Math.round(n / 48)));
    let max = 0;
    for (const v of p) max = Math.max(max, v);
    if (max <= 0) return { split: false, ratio: 1 };
    const lo = Math.floor(n * 0.3);
    const hi = Math.ceil(n * 0.7);
    let dipI = lo;
    for (let i = lo; i < hi; i++) if (p[i] < p[dipI]) dipI = i;
    let peakA = 0;
    let peakB = 0;
    for (let i = 0; i < dipI; i++) peakA = Math.max(peakA, p[i]);
    for (let i = dipI + 1; i < n; i++) peakB = Math.max(peakB, p[i]);
    const ref = Math.min(peakA, peakB);
    if (ref < max * MIN_PEAK || ref <= 0) return { split: false, ratio: 1 };
    const ratio = p[dipI] / ref;
    return { split: ratio < DIP_RATIO, ratio };
  }

  /**
   * @param {ArrayLike<number>} luma row-major luminance, 0..1
   * @returns {{cols:1|2, rows:1|2, confidence:number}|null} null when the frame is too dark to judge
   */
  function detectGrid(luma, w, h) {
    if (!luma || !w || !h || luma.length < w * h) return null;
    const colP = new Float64Array(w);
    const rowP = new Float64Array(h);
    let total = 0;
    for (let y = 0; y < h; y++) {
      for (let x = 0; x < w; x++) {
        const v = luma[y * w + x];
        colP[x] += v;
        rowP[y] += v;
        total += v;
      }
    }
    if (total / (w * h) < MIN_MEAN) return null;
    for (let x = 0; x < w; x++) colP[x] /= h;
    for (let y = 0; y < h; y++) rowP[y] /= w;
    const c = splitProfile(colP);
    const r = splitProfile(rowP);
    const conf = (s) => Math.min(1, Math.abs(s.ratio - DIP_RATIO) / DIP_RATIO);
    return { cols: c.split ? 2 : 1, rows: r.split ? 2 : 1, confidence: Math.min(conf(c), conf(r)) };
  }

  const LAYOUTS = ['sbs', 'tb', 'quad', 'mono'];

  function gridToLayout(g) {
    if (g.cols === 2) return g.rows === 2 ? 'quad' : 'sbs';
    return g.rows === 2 ? 'tb' : 'mono';
  }

  /**
   * Turn the (possibly 'auto') layout/rotation settings into concrete values.
   * @param {object} o
   * @param {string} o.layout 'auto' | 'sbs' | 'tb' | 'quad' | 'mono'
   * @param {string|number} o.rotation 'auto' | 0 | 90 | 180 | 270
   * @param {number} [o.autoRotation] brand/model default used when rotation is 'auto'
   * @param {number} o.frameW raw frame width
   * @param {number} o.frameH raw frame height
   * @param {{cols:number, rows:number}|null} [o.grid] detected grid in the raw (unrotated) frame
   * @param {boolean} [o.singleEye] the stream is already cropped to one eye
   * @returns {{layout:string, rotation:number, rotationAuto:boolean, detected:boolean}}
   */
  function resolveView(o) {
    const rotationAuto = o.rotation === 'auto' || o.rotation == null || o.rotation === '';
    let rot = ((((Math.round(Number(rotationAuto ? o.autoRotation : o.rotation) / 90) || 0) % 4) + 4) % 4) * 90;
    const g0 = o.grid && !o.singleEye ? o.grid : null;
    let g = g0 && (rot % 180 ? { cols: g0.rows, rows: g0.cols } : { cols: g0.cols, rows: g0.rows });
    // Wide frames are side-by-side panels; a horizontal split there is just a dark band in the scene.
    const baseAspect = rot % 180 ? o.frameH / o.frameW : o.frameW / o.frameH;
    if (g && baseAspect >= 1.6) g = { cols: g.cols, rows: 1 };
    // Stereo views stacked vertically are almost certainly a sideways panel: turn them side by side.
    if (rotationAuto && g && g.cols === 1 && g.rows === 2 && (o.layout === 'auto' || o.layout === 'sbs' || !o.layout)) {
      rot = (rot + 90) % 360;
      g = { cols: 2, rows: 1 };
    }
    const LW = rot % 180 ? o.frameH : o.frameW;
    const LH = rot % 180 ? o.frameW : o.frameH;
    if (LAYOUTS.includes(o.layout)) return { layout: o.layout, rotation: rot, rotationAuto, detected: false };
    if (o.singleEye) return { layout: 'mono', rotation: rot, rotationAuto, detected: false };
    let layout = g ? gridToLayout(g) : null;
    // Wide frames are side-by-side stereo even when the gap between the eyes is too faint to see.
    if ((!layout || layout === 'mono') && LH > 0 && LW / LH >= 1.6) layout = 'sbs';
    return { layout: layout || 'mono', rotation: rot, rotationAuto, detected: !!g };
  }

  return { detectGrid, splitProfile, resolveView, gridToLayout, LAYOUTS };
});
