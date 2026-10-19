//! Integer/decimal helpers for wei and token base units.
//!
//! Amounts are stored as decimal strings (exact) next to `f64` convenience
//! columns used for charts and aggregation.

use ruint::aliases::U256;

pub const WEI_PER_NATIVE: u128 = 1_000_000_000_000_000_000;

/// Parse a decimal or `0x` hex integer.
pub fn parse_uint(s: &str) -> Option<U256> {
    let s = s.trim().trim_matches('"');
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        if hex.is_empty() {
            return Some(U256::ZERO);
        }
        U256::from_str_radix(hex, 16).ok()
    } else {
        U256::from_str_radix(s, 10).ok()
    }
}

/// Parse a decimal string in whole units (e.g. `"0.000076937196976306"`) into base units.
pub fn parse_decimal_units(s: &str, decimals: u32) -> Option<U256> {
    let s = s.trim().trim_matches('"');
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    if int.is_empty() && frac.is_empty() {
        return None;
    }
    if !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut frac = frac.to_string();
    if frac.len() > decimals as usize {
        frac.truncate(decimals as usize);
    }
    while frac.len() < decimals as usize {
        frac.push('0');
    }
    let digits = format!("{}{}", if int.is_empty() { "0" } else { int }, frac);
    U256::from_str_radix(&digits, 10).ok()
}

/// Lossy conversion of base units to whole units.
pub fn to_f64(v: U256, decimals: u32) -> f64 {
    let as_f: f64 = v.to_string().parse().unwrap_or(f64::NAN);
    as_f / 10f64.powi(decimals as i32)
}

pub fn dec_to_f64(s: &str, decimals: u32) -> Option<f64> {
    parse_uint(s).map(|v| to_f64(v, decimals))
}

/// Exact formatting of base units as a decimal with up to `max_frac` fractional digits.
pub fn format_units(v: U256, decimals: u32, max_frac: usize) -> String {
    let s = v.to_string();
    let d = decimals as usize;
    let (int, frac) = if s.len() > d {
        (s[..s.len() - d].to_string(), s[s.len() - d..].to_string())
    } else {
        ("0".to_string(), format!("{s:0>d$}"))
    };
    let mut frac: String = frac.chars().take(max_frac).collect();
    while frac.ends_with('0') {
        frac.pop();
    }
    if frac.is_empty() { int } else { format!("{int}.{frac}") }
}

/// Compact human formatting of a float with ~`sig` significant digits.
pub fn fmt_sig(v: f64, sig: usize) -> String {
    if !v.is_finite() {
        return "–".into();
    }
    if v == 0.0 {
        return "0".into();
    }
    let mag = v.abs().log10().floor() as i32;
    let decimals = (sig as i32 - 1 - mag).clamp(0, 18) as usize;
    let s = format!("{v:.decimals$}");
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
}

pub fn fmt_usd(v: f64) -> String {
    if !v.is_finite() {
        return "–".into();
    }
    let sign = if v < 0.0 { "−" } else { "" };
    let a = v.abs();
    if a >= 100.0 {
        format!("{sign}${a:.0}")
    } else if a >= 0.01 {
        format!("{sign}${a:.2}")
    } else {
        format!("{sign}${}", fmt_sig(a, 2))
    }
}

/// `a * b / c` without intermediate overflow (512-bit intermediate is not needed for
/// the magnitudes involved: amounts < 2^128, multiplier 1e18).
pub fn mul_div(a: U256, b: U256, c: U256) -> Option<U256> {
    if c.is_zero() {
        return None;
    }
    a.checked_mul(b).map(|p| p / c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_and_decimal() {
        assert_eq!(parse_uint("0x17f4b").unwrap(), U256::from(98_123u64));
        assert_eq!(parse_uint("106983915816351").unwrap(), U256::from(106_983_915_816_351u64));
        assert_eq!(parse_uint("0xe8b3e728c9c0000").unwrap(), U256::from(1_048_000_000_000_000_000u128));
    }

    #[test]
    fn parses_decimal_units() {
        assert_eq!(parse_decimal_units("0.000106983915816351", 18).unwrap(), U256::from(106_983_915_816_351u64));
        assert_eq!(parse_decimal_units("1.5", 6).unwrap(), U256::from(1_500_000u64));
        assert_eq!(parse_decimal_units("2", 2).unwrap(), U256::from(200u64));
        assert!(parse_decimal_units("abc", 18).is_none());
    }

    #[test]
    fn formats_units() {
        assert_eq!(format_units(U256::from(106_983_915_816_351u64), 18, 9), "0.000106983");
        assert_eq!(format_units(U256::from(1_500_000u64), 6, 6), "1.5");
        assert_eq!(format_units(U256::from(3u64), 0, 6), "3");
        assert_eq!(fmt_sig(0.000038046, 3), "0.000038");
        assert_eq!(fmt_sig(1234.6, 3), "1235");
    }
}
