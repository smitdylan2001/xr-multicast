// Stream resolution matched to the size a stream is actually shown at.
//
// Viewers (the hub window and remote tablets) report, per stream, how each tile
// is displayed. From that and the headset's panel size we compute the encoded
// resolution needed for ~1 source pixel per screen pixel, then round UP to a
// fixed step so small resizes don't cause restarts.

/** Long-side resolution steps; the panel's native size is always an extra final step. */
const RES_STEPS = [640, 960, 1280, 1600, 1920, 2560, 3200, 3840];

const EYE_RECT = {
  sbs: { w: 0.5, h: 1 },
  tb: { w: 1, h: 0.5 },
  quad: { w: 0.5, h: 0.5 },
  mono: { w: 1, h: 1 },
};

function parseAspect(v, fallback) {
  const m = String(v || '').match(/^(\d+(?:\.\d+)?):(\d+(?:\.\d+)?)$/);
  return m ? Number(m[1]) / Number(m[2]) : fallback;
}

/** Height of a box with aspect `a` fitted inside w×h. */
function fitHeight(a, w, h) {
  return Math.min(h, w / a);
}

/**
 * Encoded long-side pixels needed to show one stream in one tile at ~1:1.
 * @param {{w:number, h:number}} frame encoded frame size in natural orientation (any proportional units)
 * @param {{visible?:boolean, tileW:number, tileH:number, mode:'corrected'|'stereo'|'raw',
 *          zoom?:number, outputAspect?:string, layout?:string, rotation?:number}} req tile size in device pixels
 * @returns {number} 0 when nothing is visible
 */
function neededLongSide(frame, req) {
  if (!req || req.visible === false || !frame || !(frame.w > 0) || !(frame.h > 0)) return 0;
  const tileW = Math.max(0, Number(req.tileW) || 0);
  const tileH = Math.max(0, Number(req.tileH) || 0);
  if (!tileW || !tileH) return 0;
  const long = Math.max(frame.w, frame.h);
  const sideways = (((Math.round((Number(req.rotation) || 0) / 90) % 4) + 4) % 4) % 2 === 1;
  const LW = sideways ? frame.h : frame.w;
  const LH = sideways ? frame.w : frame.h;

  if (req.mode === 'raw') return long * Math.min(tileW / LW, tileH / LH);

  const eye = EYE_RECT[req.layout] || EYE_RECT.mono;
  const eyeAspect = (LW * eye.w) / (LH * eye.h);
  const outAspect = parseAspect(req.outputAspect, eyeAspect);
  const zoom = Math.max(0.1, Number(req.zoom) || 1);
  // Stereo draws two eyes side by side, each at most square (see lens-renderer.js).
  const stereo = req.mode === 'stereo' && req.layout && req.layout !== 'mono';
  const vpH = stereo ? fitHeight(Math.min(outAspect, 1) * 2, tileW, tileH) : fitHeight(outAspect, tileW, tileH);
  // The corrected view shows eyeHeight/zoom of the eye across the viewport height (1:1 at the lens centre).
  const neededEyeH = vpH * zoom;
  const neededLH = neededEyeH / eye.h;
  return long * (neededLH / LH);
}

/**
 * Smallest step at or above `need`, capped by the quality preset and the panel's native size.
 * @param {number} need encoded long side needed (0 = hidden)
 * @param {{cap?:number, nativeLong?:number}} o cap: preset max long side (0/undefined = native)
 * @returns {number} chosen long side in pixels (== nativeLong means "native", i.e. max_size=0)
 */
function pickStep(need, { cap, nativeLong } = {}) {
  const native = nativeLong > 0 ? nativeLong : Infinity;
  const ceiling = Math.min(cap > 0 ? cap : Infinity, native);
  const steps = RES_STEPS.filter((s) => s < native);
  if (Number.isFinite(native)) steps.push(native);
  let chosen = steps.find((s) => s >= need);
  if (chosen === undefined) chosen = steps[steps.length - 1];
  if (chosen > ceiling) chosen = Number.isFinite(ceiling) ? ceiling : chosen;
  return Math.max(chosen, Math.min(RES_STEPS[0], ceiling));
}

/** Index on the step ladder (for "how many steps apart" decisions). */
function stepIndex(size, nativeLong) {
  const steps = RES_STEPS.filter((s) => !(nativeLong > 0) || s < nativeLong);
  if (nativeLong > 0) steps.push(nativeLong);
  let idx = steps.findIndex((s) => s >= size);
  if (idx === -1) idx = steps.length - 1;
  return idx;
}

/**
 * Bitrate for a reduced resolution: scales with pixel count^0.75 relative to the
 * preset's cap, with a floor so small tiles still look clean.
 */
function scaledBitrate(base, size, capLong, floor = 1_500_000) {
  if (!(capLong > 0) || !(size > 0) || size >= capLong) return base;
  const scaled = base * Math.pow(size / capLong, 1.5);
  return Math.round(Math.min(base, Math.max(Math.min(floor, base), scaled)));
}

/**
 * Whether a running stream should be restarted at a new size.
 * Upgrades apply immediately; downgrades only when at least 2 steps lower and
 * persistent for `downDelay` ms (longer when no viewer shows the stream).
 * @returns {{action:'none'|'resize'|'wait', waitMs?:number}}
 */
function decideResize({ current, target, nativeLong, now, downSince, hiddenOnly, downDelay = 5000, hiddenDelay = 15000 }) {
  const ci = stepIndex(current, nativeLong);
  const ti = stepIndex(target, nativeLong);
  if (ti > ci) return { action: 'resize' };
  if (ci - ti < 2) return { action: 'none' };
  const delay = hiddenOnly ? hiddenDelay : downDelay;
  if (downSince == null) return { action: 'wait', waitMs: delay };
  const left = downSince + delay - now;
  return left <= 0 ? { action: 'resize' } : { action: 'wait', waitMs: left };
}

module.exports = { RES_STEPS, neededLongSide, pickStep, stepIndex, scaledBitrate, decideResize };
