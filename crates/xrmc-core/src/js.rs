//! Small helpers that reproduce JavaScript number semantics, so values computed
//! here match the Electron reference implementation bit for bit.

/// `Math.round`: nearest integer, ties towards +∞ (Rust's `f64::round` rounds
/// ties away from zero, which differs for negative halves).
pub(crate) fn round(x: f64) -> f64 {
    let f = x.floor();
    if x - f >= 0.5 { f + 1.0 } else { f }
}

/// `Number.prototype.toFixed(digits)` for finite values of moderate size.
///
/// Rust's `{:.N}` rounds exact ties to even, JavaScript rounds them away from
/// zero. A tie at `N` decimals can only happen when `x · 2^(N+1)` is an odd
/// integer, so that case is nudged before formatting.
pub(crate) fn to_fixed(x: f64, digits: usize) -> String {
    let scaled = x * f64::powi(2.0, digits as i32 + 1);
    let tie = scaled.fract() == 0.0 && (scaled.abs() % 2.0) == 1.0;
    let v = if tie {
        x + x.signum() * 0.25 * 10f64.powi(-(digits as i32))
    } else {
        x
    };
    format!("{v:.digits$}")
}

/// Formats a number the way JavaScript's `String(n)` does for the values used
/// in labels (integers print without a fractional part).
pub(crate) fn num_to_string(x: f64) -> String {
    if x.is_finite() && x.fract() == 0.0 && x.abs() < 1e15 {
        format!("{}", x as i64)
    } else {
        format!("{x}")
    }
}

/// Case-insensitive-first string ordering, an approximation of
/// `String.prototype.localeCompare` for sorting names.
pub(crate) fn locale_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase()
        .cmp(&b.to_lowercase())
        .then_with(|| a.cmp(b))
}

/// Milliseconds since the Unix epoch (`Date.now()`).
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn math_round_ties_go_up() {
        assert_eq!(round(2.5), 3.0);
        assert_eq!(round(-2.5), -2.0);
        assert_eq!(round(-0.5), 0.0);
        assert_eq!(round(0.49999999999999994), 0.0);
        assert!(round(f64::NAN).is_nan());
    }

    #[test]
    fn to_fixed_matches_javascript() {
        assert_eq!(to_fixed(2.25, 1), "2.3");
        assert_eq!(to_fixed(2.75, 1), "2.8");
        assert_eq!(to_fixed(1.15, 1), "1.1"); // 1.15 is stored slightly below the tie
        assert_eq!(to_fixed(2.35, 1), "2.4");
        assert_eq!(to_fixed(8.0, 1), "8.0");
        assert_eq!(to_fixed(-2.25, 1), "-2.3");
        assert_eq!(to_fixed(0.125, 2), "0.13");
    }

    #[test]
    fn numbers_print_like_javascript() {
        assert_eq!(num_to_string(8.0), "8");
        assert_eq!(num_to_string(12.5), "12.5");
        assert_eq!(num_to_string(0.5), "0.5");
    }
}
