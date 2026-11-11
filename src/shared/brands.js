// Headset vendor definitions: detection, labelling, setup instructions and
// the starting lens-correction profile for each family.
//
// About the correction values: standalone headsets mirror their *panel* image,
// i.e. two eye views that are already barrel-distorted (and chroma-shifted) so
// they look right through the lenses. The renderer undoes that with a radial
// model  r_src = r * (1 + k1·r² + k2·r⁴)  (r = 1 at half the eye height).
// These presets are sensible starting points per family; every headset can be
// fine-tuned in the Calibrate panel and the result is saved per device.
//
// A brand can list `variants` for models that need different defaults; a
// variant's `correction` is merged over the brand's (see presetCorrection).

const BASE_CORRECTION = {
  layout: 'auto', // 'auto' (detected from the video) | 'sbs' | 'tb' (top/bottom) | 'quad' (2×2 grid) | 'mono'
  quadRow: 'top', // which row of a 2×2 grid holds the eye pair
  eye: 'left', // which eye to show in single-eye corrected mode
  k1: -0.2,
  k2: 0.03,
  zoom: 1.4,
  centerX: 0.0, // lens centre offset for the LEFT eye (right eye is mirrored)
  centerY: 0.0,
  chromaR: 1.0, // per-channel radial scale, for lateral chromatic aberration
  chromaB: 1.0,
  rotation: 'auto', // 'auto' | 0 | 90 | 180 | 270 (clockwise)
  autoRotation: 0, // what 'auto' starts from; +90 more when the eye views come out stacked vertically
  flipH: false,
  flipV: false,
  outputAspect: '16:9', // '1:1' | '4:3' | '16:9' | 'eye'
};

const BRANDS = {
  meta: {
    id: 'meta',
    name: 'Meta',
    family: 'Meta Quest',
    color: '#1877F2',
    match: /\b(oculus|meta)\b|quest|hollywood|eureka|panther|seacliff|monterey/i,
    models: {
      monterey: 'Quest',
      hollywood: 'Quest 2',
      seacliff: 'Quest Pro',
      eureka: 'Quest 3',
      panther: 'Quest 3S',
    },
    correction: { ...BASE_CORRECTION, k1: -0.22, k2: 0.04, zoom: 1.45, centerX: 0.04 },
    setup: [
      'Create/sign in to a Meta developer account (developers.meta.com) and join or create an organization.',
      'In the Meta Horizon phone app: Menu → Devices → select the headset → Headset settings → Developer mode → On.',
      'Reboot the headset.',
      'Windows only: install the "Oculus ADB Drivers" from developers.meta.com/horizon/downloads.',
      'Plug the headset in with a USB-C data cable, put it on, and accept "Allow USB debugging" (tick "Always allow from this computer").',
    ],
  },
  pico: {
    id: 'pico',
    name: 'PICO',
    family: 'PICO',
    color: '#00B8A9',
    match: /pico|bytedance/i,
    models: {
      a8110: 'PICO 4',
      a9210: 'PICO 4 Ultra',
    },
    correction: { ...BASE_CORRECTION, k1: -0.2, k2: 0.03, zoom: 1.4 },
    setup: [
      'On the headset: Settings → General → About → tap "Software Version" 7 times until Developer options appear.',
      'Settings → Developer → enable "USB Debugging".',
      'Plug in via USB-C, put the headset on and accept the USB debugging prompt (tick "Always allow").',
    ],
  },
  yvr: {
    id: 'yvr',
    name: 'Play For Dream',
    family: 'Play For Dream / YVR',
    color: '#8B5CF6',
    match: /\byvr\b|play\s*for\s*dream|playfordream|pfdm|dreamsmart/i,
    models: {},
    variants: [
      // Display 0 is 2856×3072: one micro-OLED eye panel (3840×3552) turned sideways.
      { id: 'pfd-mr', match: /pfdm|yvr_d3/i, model: 'MR', correction: { autoRotation: 90 } },
    ],
    correction: { ...BASE_CORRECTION, k1: -0.2, k2: 0.03, zoom: 1.4 },
    setup: [
      'On the headset: Settings → About → tap the build / version number 7 times to unlock Developer options.',
      'Open Developer options and enable "USB debugging".',
      'Plug in via USB-C, put the headset on and accept the USB debugging prompt (tick "Always allow").',
    ],
  },
  htc: {
    id: 'htc',
    name: 'HTC VIVE',
    family: 'HTC VIVE (Focus / XR Elite)',
    color: '#00A3E0',
    match: /\bhtc\b|vive/i,
    models: {},
    correction: { ...BASE_CORRECTION, k1: -0.21, k2: 0.035, zoom: 1.4 },
    setup: [
      'On the headset: Settings → Advanced (or About) → tap "Build number" / version 7 times to unlock Developer options.',
      'Open Developer options and enable "USB debugging".',
      'Plug in via USB-C, put the headset on and accept the USB debugging prompt (tick "Always allow").',
    ],
  },
  androidxr: {
    id: 'androidxr',
    name: 'Android XR',
    family: 'Android XR (e.g. Galaxy XR)',
    color: '#34A853',
    match: /galaxy\s*xr|sm-i61\d|moohan|android\s*xr/i,
    models: {
      'sm-i610': 'Galaxy XR',
    },
    correction: { ...BASE_CORRECTION, k1: -0.18, k2: 0.025, zoom: 1.35 },
    setup: [
      'On the headset: Settings → About → Software information → tap "Build number" 7 times.',
      'Settings → Developer options → enable "USB debugging".',
      'Plug in via USB-C, put the headset on and accept the USB debugging prompt (tick "Always allow").',
    ],
  },
  android: {
    id: 'android',
    name: 'Android',
    family: 'Other Android device',
    color: '#6B7280',
    match: null,
    models: {},
    correction: { ...BASE_CORRECTION, layout: 'mono', k1: 0, k2: 0, zoom: 1, outputAspect: 'eye' },
    setup: [
      'Settings → About → tap "Build number" 7 times, then enable Developer options → USB debugging.',
      'Plug in via USB and accept the USB debugging prompt.',
    ],
  },
};

const XR_FEATURE_RE = /android\.(software|hardware)\.xr/i;

/**
 * @param {{manufacturer:string, brand:string, model:string, device:string, product:string, features:string[]}} info
 */
function detectBrand(info) {
  const haystack = [info.manufacturer, info.brand, info.model, info.device, info.product].filter(Boolean).join(' ');
  for (const id of ['meta', 'pico', 'yvr', 'htc', 'androidxr']) {
    if (BRANDS[id].match.test(haystack)) return id;
  }
  if ((info.features || []).some((f) => XR_FEATURE_RE.test(f))) return 'androidxr';
  return 'android';
}

/** Id of the brand variant matching this device's model/device/product, or null. */
function detectVariant(brandId, info) {
  const brand = BRANDS[brandId];
  if (!brand || !brand.variants || !info) return null;
  const haystack = [info.model, info.device, info.product].filter(Boolean).join(' ');
  const v = haystack && brand.variants.find((x) => x.match.test(haystack));
  return v ? v.id : null;
}

/** Starting correction for a brand, with the variant's overrides applied. */
function presetCorrection(brandId, variantId) {
  const brand = BRANDS[brandId] || BRANDS.android;
  const v = variantId && (brand.variants || []).find((x) => x.id === variantId);
  return { ...brand.correction, ...((v && v.correction) || {}) };
}

function friendlyModel(brandId, info) {
  const brand = BRANDS[brandId] || BRANDS.android;
  const variant = (brand.variants || []).find((x) => x.id === detectVariant(brandId, info));
  if (variant && variant.model) return variant.model;
  const keys = [info.device, info.model, info.product].filter(Boolean).map((s) => s.toLowerCase());
  for (const k of keys) {
    if (brand.models[k]) return brand.models[k];
  }
  return (info.model || info.device || 'Unknown').replace(/_/g, ' ');
}

module.exports = { BRANDS, BASE_CORRECTION, detectBrand, detectVariant, presetCorrection, friendlyModel };
