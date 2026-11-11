//! Headset vendor definitions: detection, labelling, setup instructions and the
//! starting lens-correction profile for each family (port of `src/shared/brands.js`).
//!
//! The brand *data* lives in `data/brands.json`, generated from the Electron
//! app's `src/shared/brands.js` by `scripts/export-brands.js`, so both apps
//! share one source of truth. A test checks the embedded file is up to date.
//!
//! About the correction values: standalone headsets mirror their *panel* image,
//! i.e. two eye views that are already barrel-distorted (and chroma-shifted) so
//! they look right through the lenses. The renderer undoes that with a radial
//! model `r_src = r · (1 + k1·r² + k2·r⁴)` (r = 1 at half the eye height).

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::bytes::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::device_info::DeviceInfo;
use crate::layout::{LayoutSetting, Rotation};

/// The embedded brand data (`data/brands.json`).
pub const BRANDS_JSON: &str = include_str!("../data/brands.json");

/// Id of the generic fallback brand.
pub const FALLBACK_BRAND: &str = "android";

/// Which eye to show in single-eye corrected mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Eye {
    #[default]
    Left,
    Right,
}

/// A lens-correction profile (the renderer's calibration values).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Correction {
    /// `auto` (detected from the video) | `sbs` | `tb` | `quad` | `mono`.
    pub layout: LayoutSetting,
    /// Which row of a 2×2 grid holds the eye pair (`top` | `bottom`).
    pub quad_row: String,
    pub eye: Eye,
    pub k1: f64,
    pub k2: f64,
    pub zoom: f64,
    /// Lens centre offset for the LEFT eye (the right eye is mirrored).
    pub center_x: f64,
    pub center_y: f64,
    /// Per-channel radial scale, for lateral chromatic aberration.
    pub chroma_r: f64,
    pub chroma_b: f64,
    pub rotation: Rotation,
    /// What `rotation: auto` starts from; +90 more when the eye views come out stacked vertically.
    pub auto_rotation: f64,
    pub flip_h: bool,
    pub flip_v: bool,
    /// `1:1` | `4:3` | `16:9` | `eye`.
    pub output_aspect: String,
}

/// A model of a brand that needs different defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrandVariant {
    pub id: String,
    /// Friendly model name, e.g. `MR`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Correction overrides merged over the brand's correction.
    #[serde(default)]
    pub correction: Map<String, Value>,
    /// Regular expression (JavaScript syntax) matched against model/device/product.
    #[serde(rename = "match")]
    pub match_source: Option<String>,
    pub match_flags: Option<String>,
    #[serde(skip)]
    matcher: Option<Regex>,
}

/// A headset vendor / family.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Brand {
    pub id: String,
    pub name: String,
    pub family: String,
    pub color: String,
    /// Lower-case device/model/product → friendly model name.
    #[serde(default)]
    pub models: BTreeMap<String, String>,
    pub correction: Correction,
    /// Steps to enable USB debugging, shown in the setup guide.
    #[serde(default)]
    pub setup: Vec<String>,
    /// Regular expression (JavaScript syntax) matched against the device properties; `None` = fallback brand.
    #[serde(rename = "match")]
    pub match_source: Option<String>,
    pub match_flags: Option<String>,
    #[serde(default)]
    pub variants: Vec<BrandVariant>,
    #[serde(skip)]
    matcher: Option<Regex>,
}

impl Brand {
    /// Whether the brand's pattern matches `haystack`.
    pub fn matches(&self, haystack: &str) -> bool {
        self.matcher
            .as_ref()
            .is_some_and(|re| re.is_match(haystack.as_bytes()))
    }

    /// The variant with this id.
    pub fn variant(&self, id: &str) -> Option<&BrandVariant> {
        self.variants.iter().find(|v| v.id == id)
    }
}

impl BrandVariant {
    pub fn matches(&self, haystack: &str) -> bool {
        self.matcher
            .as_ref()
            .is_some_and(|re| re.is_match(haystack.as_bytes()))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BrandFile {
    base_correction: Correction,
    brands: Vec<Brand>,
}

/// Compile a JavaScript regex (source + flags) for byte matching.
///
/// Unicode mode is off so `\b`, `\w` and case folding are ASCII, like a
/// JavaScript regex without the `u` flag. Patterns that only compile in
/// Unicode mode fall back to it.
fn compile(source: Option<&str>, flags: Option<&str>) -> Option<Regex> {
    let source = source?;
    let flags = flags.unwrap_or("");
    let build = |unicode: bool| {
        RegexBuilder::new(source)
            .case_insensitive(flags.contains('i'))
            .multi_line(flags.contains('m'))
            .dot_matches_new_line(flags.contains('s'))
            .unicode(unicode)
            .build()
    };
    match build(false).or_else(|_| build(true)) {
        Ok(re) => Some(re),
        Err(e) => {
            tracing::error!("brands: invalid pattern {source:?}: {e}");
            None
        }
    }
}

struct BrandTable {
    base_correction: Correction,
    brands: Vec<Brand>,
}

static TABLE: LazyLock<BrandTable> = LazyLock::new(|| {
    // The embedded file is generated and covered by tests; it is not user input.
    let mut file: BrandFile =
        serde_json::from_str(BRANDS_JSON).expect("embedded data/brands.json is valid");
    for b in &mut file.brands {
        b.matcher = compile(b.match_source.as_deref(), b.match_flags.as_deref());
        for v in &mut b.variants {
            v.matcher = compile(v.match_source.as_deref(), v.match_flags.as_deref());
        }
    }
    BrandTable {
        base_correction: file.base_correction,
        brands: file.brands,
    }
});

static XR_FEATURE_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)android\.(software|hardware)\.xr").expect("valid regex")
});

/// All brands, in definition order (the fallback `android` brand last).
pub fn brands() -> &'static [Brand] {
    &TABLE.brands
}

/// The base correction every brand preset starts from.
pub fn base_correction() -> &'static Correction {
    &TABLE.base_correction
}

/// A brand by id.
pub fn brand(id: &str) -> Option<&'static Brand> {
    TABLE.brands.iter().find(|b| b.id == id)
}

/// A brand by id, or the generic Android brand (`BRANDS[id] || BRANDS.android`).
pub fn brand_or_fallback(id: &str) -> &'static Brand {
    brand(id)
        .or_else(|| brand(FALLBACK_BRAND))
        .or_else(|| TABLE.brands.last())
        .expect("embedded brand table is not empty")
}

fn join_present(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Which brand a device belongs to, from its properties and XR features.
pub fn detect_brand(info: &DeviceInfo) -> &'static str {
    let haystack = join_present(&[
        &info.manufacturer,
        &info.brand,
        &info.model,
        &info.device,
        &info.product,
    ]);
    if let Some(b) = TABLE.brands.iter().find(|b| b.matches(&haystack)) {
        return &b.id;
    }
    if info.features.iter().any(|f| XR_FEATURE_RE.is_match(f)) {
        return "androidxr";
    }
    FALLBACK_BRAND
}

/// Id of the brand variant matching this device's model/device/product.
pub fn detect_variant(brand_id: &str, info: &DeviceInfo) -> Option<&'static str> {
    let brand = brand(brand_id)?;
    if brand.variants.is_empty() {
        return None;
    }
    let haystack = join_present(&[&info.model, &info.device, &info.product]);
    if haystack.is_empty() {
        return None;
    }
    brand
        .variants
        .iter()
        .find(|v| v.matches(&haystack))
        .map(|v| v.id.as_str())
}

/// Starting correction for a brand, with the variant's overrides applied.
pub fn preset_correction(brand_id: &str, variant_id: Option<&str>) -> Correction {
    let brand = brand_or_fallback(brand_id);
    let Some(v) = variant_id.and_then(|id| brand.variant(id)) else {
        return brand.correction.clone();
    };
    let Ok(Value::Object(mut merged)) = serde_json::to_value(&brand.correction) else {
        return brand.correction.clone();
    };
    for (k, val) in &v.correction {
        merged.insert(k.clone(), val.clone());
    }
    serde_json::from_value(Value::Object(merged)).unwrap_or_else(|e| {
        tracing::warn!("brands: variant {} has an invalid correction: {e}", v.id);
        brand.correction.clone()
    })
}

/// Friendly model name, e.g. `Quest 3` or `MR`.
pub fn friendly_model(brand_id: &str, info: &DeviceInfo) -> String {
    let brand = brand_or_fallback(brand_id);
    if let Some(model) = detect_variant(brand_id, info)
        .and_then(|id| brand.variant(id))
        .and_then(|v| v.model.as_deref())
        .filter(|m| !m.is_empty())
    {
        return model.to_string();
    }
    for key in [&info.device, &info.model, &info.product] {
        if key.is_empty() {
            continue;
        }
        if let Some(m) = brand.models.get(&key.to_lowercase()) {
            return m.clone();
        }
    }
    let raw = [&info.model, &info.device]
        .into_iter()
        .find(|s| !s.is_empty())
        .map_or("Unknown", |s| s.as_str());
    raw.replace('_', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(
        manufacturer: &str,
        brand: &str,
        model: &str,
        device: &str,
        product: &str,
    ) -> DeviceInfo {
        DeviceInfo {
            manufacturer: manufacturer.into(),
            brand: brand.into(),
            model: model.into(),
            device: device.into(),
            product: product.into(),
            ..Default::default()
        }
    }

    // Port of the brand test in test/layout-detect.test.js.
    #[test]
    fn pfd_mr_is_detected_as_a_play_for_dream_mr_with_its_own_preset() {
        let i = info("Play For Dream", "YVR", "PFDM MR", "yvr_d3", "yvr_d3");
        let b = detect_brand(&i);
        assert_eq!(b, "yvr");
        assert_eq!(detect_variant(b, &i), Some("pfd-mr"));
        assert_eq!(
            format!("{} {}", brand(b).unwrap().name, friendly_model(b, &i)),
            "Play For Dream MR"
        );
        let c = preset_correction(b, Some("pfd-mr"));
        assert_eq!(c.auto_rotation, 90.0);
        assert_eq!(c.rotation, Rotation::Auto);
        assert_eq!(c.layout, LayoutSetting::Auto);
        assert_eq!(preset_correction("meta", None).auto_rotation, 0.0);
        assert_eq!(
            detect_variant("meta", &info("", "", "Quest 3", "eureka", "")),
            None
        );
    }

    #[test]
    fn quest_models_are_named() {
        let q3 = info("Oculus", "oculus", "Quest 3", "eureka", "eureka");
        assert_eq!(detect_brand(&q3), "meta");
        assert_eq!(friendly_model("meta", &q3), "Quest 3");
        let q2 = info("Oculus", "oculus", "Quest 2", "hollywood", "hollywood");
        assert_eq!(friendly_model("meta", &q2), "Quest 2");
        let q3s = info("Oculus", "oculus", "Quest 3S", "panther", "panther");
        assert_eq!(friendly_model("meta", &q3s), "Quest 3S");
        assert_eq!(detect_brand(&info("Meta", "", "", "", "")), "meta");
    }

    #[test]
    fn other_brands_are_detected() {
        let pico = info("Pico", "Pico", "A8110", "PICO 4", "PICO 4");
        assert_eq!(detect_brand(&pico), "pico");
        assert_eq!(friendly_model("pico", &pico), "PICO 4");
        assert_eq!(detect_brand(&info("ByteDance", "", "", "", "")), "pico");
        assert_eq!(
            detect_brand(&info("HTC", "htc", "VIVE Focus 3", "", "")),
            "htc"
        );
        assert_eq!(
            detect_brand(&info("samsung", "samsung", "SM-I610", "moohan", "")),
            "androidxr"
        );
        assert_eq!(
            friendly_model("androidxr", &info("samsung", "samsung", "SM-I610", "", "")),
            "Galaxy XR"
        );
        assert_eq!(
            detect_brand(&info("Google", "google", "Pixel 9", "tokay", "tokay")),
            "android"
        );
        assert_eq!(
            friendly_model("android", &info("Google", "google", "", "my_device", "")),
            "my device"
        );
        assert_eq!(friendly_model("android", &DeviceInfo::default()), "Unknown");
        // `\b` is an ASCII word boundary: "metal" is not Meta.
        assert_eq!(detect_brand(&info("Metalworks", "", "", "", "")), "android");
    }

    #[test]
    fn xr_features_mean_android_xr() {
        let mut i = info("Acme", "acme", "Visor", "visor", "visor");
        i.features = vec!["android.software.xr.api.spatial".into()];
        assert_eq!(detect_brand(&i), "androidxr");
    }

    #[test]
    fn brand_table_is_complete() {
        let ids: Vec<_> = brands().iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, ["meta", "pico", "yvr", "htc", "androidxr", "android"]);
        assert!(
            brands()
                .iter()
                .all(|b| b.match_source.is_none() || b.matcher.is_some())
        );
        assert_eq!(brand_or_fallback("nope").id, "android");
        assert_eq!(
            preset_correction("nope", None),
            brand("android").unwrap().correction
        );
        assert_eq!(base_correction().zoom, 1.4);
        let c = preset_correction("android", Some("nope"));
        assert_eq!(c.layout, LayoutSetting::Mono);
        assert_eq!(c.output_aspect, "eye");
    }

    #[test]
    fn corrections_serialise_with_electron_field_names() {
        let v = serde_json::to_value(preset_correction("meta", None)).unwrap();
        assert_eq!(v["flipH"], Value::Bool(false));
        assert_eq!(v["centerX"], serde_json::json!(0.04));
        assert_eq!(v["rotation"], Value::String("auto".into()));
    }

    /// The embedded JSON must match a fresh export of `src/shared/brands.js`.
    #[test]
    fn embedded_brands_match_the_electron_source() {
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/export-brands.js");
        let node = std::env::var("NODE").unwrap_or_else(|_| "node".into());
        let out = match std::process::Command::new(&node)
            .arg(script)
            .arg("--stdout")
            .output()
        {
            Ok(o) if o.status.success() => o.stdout,
            Ok(o) => {
                // The Electron sources are not part of every checkout (e.g. a vendored crate).
                eprintln!(
                    "skipping: export-brands.js failed: {}",
                    String::from_utf8_lossy(&o.stderr)
                );
                return;
            }
            Err(e) => {
                eprintln!("skipping: node is not available ({e})");
                return;
            }
        };
        let fresh: Value = serde_json::from_slice(&out).expect("export is JSON");
        let embedded: Value = serde_json::from_str(BRANDS_JSON).unwrap();
        assert_eq!(
            fresh, embedded,
            "data/brands.json is stale: run `node crates/xrmc-core/scripts/export-brands.js`"
        );
    }
}
