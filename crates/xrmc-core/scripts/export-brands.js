#!/usr/bin/env node
// Exports the headset brand definitions from the Electron app (src/shared/brands.js)
// to crates/xrmc-core/data/brands.json, so the Rust core and the Electron app share
// one source of truth. Regular expressions are written as their source string plus
// a flags string.
//
//   node crates/xrmc-core/scripts/export-brands.js            # (re)write data/brands.json
//   node crates/xrmc-core/scripts/export-brands.js --stdout   # print the JSON instead
'use strict';

const fs = require('fs');
const path = require('path');

const { BRANDS, BASE_CORRECTION } = require(path.join(__dirname, '..', '..', '..', 'src', 'shared', 'brands.js'));

function regex(re) {
  if (!re) return { match: null, matchFlags: null };
  return { match: re.source, matchFlags: re.flags };
}

function exportBrand(b) {
  const { match, variants, ...rest } = b;
  return {
    ...rest,
    ...regex(match),
    variants: (variants || []).map((v) => {
      const { match: vm, ...vrest } = v;
      return { ...vrest, ...regex(vm) };
    }),
  };
}

const out = {
  $comment: 'Generated from src/shared/brands.js by crates/xrmc-core/scripts/export-brands.js. Do not edit by hand.',
  baseCorrection: BASE_CORRECTION,
  // An array (not an object) so the brand order survives every JSON parser.
  brands: Object.values(BRANDS).map(exportBrand),
};

const json = `${JSON.stringify(out, null, 2)}\n`;
if (process.argv.includes('--stdout')) {
  process.stdout.write(json);
} else {
  const file = path.join(__dirname, '..', 'data', 'brands.json');
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, json);
  console.log(`Wrote ${path.relative(process.cwd(), file)}`);
}
