//! Wei to native-unit strings, so a consumer reads a ceiling as ether instead of doing the
//! eighteen-decimal shift itself. Every EVM chain prices gas in an eighteen-decimal unit.

use serde_json::{json, Value};

pub const NATIVE_DECIMALS: u8 = 18;
const DISPLAY_PLACES: u32 = 5;

/// Every digit, trailing zeros trimmed: `54663000000000` → `"0.000054663"`.
pub fn format_exact(wei: u128, decimals: u8) -> String {
    let digits = wei.to_string();
    let d = decimals as usize;
    if d == 0 {
        return digits;
    }
    let padded = if digits.len() <= d { format!("{}{digits}", "0".repeat(d + 1 - digits.len())) } else { digits };
    let split = padded.len() - d;
    let int_part = padded[..split].trim_start_matches('0');
    let int_part = if int_part.is_empty() { "0" } else { int_part };
    let frac = padded[split..].trim_end_matches('0');
    if frac.is_empty() { int_part.to_string() } else { format!("{int_part}.{frac}") }
}

/// At most five places, truncated rather than rounded, and `"<0.00001"` for dust: a fee
/// shown as more than it can be is worse than one shown as less, and only zero reads `"0"`.
pub fn format_display(wei: u128, decimals: u8) -> String {
    if wei == 0 {
        return "0".to_string();
    }
    let p = DISPLAY_PLACES.min(u32::from(decimals));
    if p == 0 {
        return wei.to_string();
    }
    let threshold = 10u128.checked_pow(u32::from(decimals) - p);
    if threshold.map(|t| wei < t).unwrap_or(true) {
        return format!("<0.{}1", "0".repeat(p as usize - 1));
    }
    let exact = format_exact(wei, decimals);
    let Some((int_part, frac)) = exact.split_once('.') else { return exact };
    let cut = frac[..frac.len().min(p as usize)].trim_end_matches('0');
    if cut.is_empty() { int_part.to_string() } else { format!("{int_part}.{cut}") }
}

/// Write `<key>` (decimal wei), `<key>Display` and `<key>Exact` beside each other.
pub fn decorate(v: &mut Value, key: &str, wei: u128) {
    v[key] = json!(wei.to_string());
    v[format!("{key}Display")] = json!(format_display(wei, NATIVE_DECIMALS));
    v[format!("{key}Exact")] = json!(format_exact(wei, NATIVE_DECIMALS));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_keeps_every_digit_and_display_truncates() {
        assert_eq!(format_exact(1_500_000_000_000_000_000, 18), "1.5");
        assert_eq!(format_display(1_500_000_000_000_000_000, 18), "1.5");
        assert_eq!(format_exact(54_663_000_000_000, 18), "0.000054663");
        assert_eq!(format_display(54_663_000_000_000, 18), "0.00005");
        assert_eq!(format_exact(123_456_789_012_345_678_901, 18), "123.456789012345678901");
        assert_eq!(format_display(123_456_789_012_345_678_901, 18), "123.45678", "truncated, never rounded up");
        assert_eq!(format_exact(100_000_000_000_000, 18), "0.0001");
        assert_eq!(format_display(100_000_000_000_000, 18), "0.0001");
    }

    #[test]
    fn only_zero_reads_zero_and_dust_is_marked() {
        assert_eq!(format_display(0, 18), "0");
        assert_eq!(format_exact(0, 18), "0");
        assert_eq!(format_display(1, 18), "<0.00001");
        assert_eq!(format_exact(1, 18), "0.000000000000000001");
        assert_eq!(format_display(9_999_999_999_999, 18), "<0.00001");
    }

    #[test]
    fn decorate_writes_the_three_readings_side_by_side() {
        let mut v = json!({});
        decorate(&mut v, "feeCeilingWei", 42_000_000_000_000);
        assert_eq!(v["feeCeilingWei"], "42000000000000");
        assert_eq!(v["feeCeilingWeiDisplay"], "0.00004");
        assert_eq!(v["feeCeilingWeiExact"], "0.000042");
    }
}
