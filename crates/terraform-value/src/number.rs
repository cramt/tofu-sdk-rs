//! An arbitrary-precision `cty` number.

use std::fmt;

/// A Terraform/`cty` number.
///
/// `cty` numbers are arbitrary precision (Go's `big.Float`). A bare `f64`
/// silently loses precision above 2^53, which corrupts 64-bit IDs, large byte
/// counts, and high-precision decimals with no error. To avoid that, this keeps
/// integers that fit `i64`/`u64` exact, decimals that `f64` represents exactly
/// as `f64`, and anything outside those (very large integers, high-precision
/// decimals) verbatim as canonical decimal text — the same string fallback
/// go-cty uses on the msgpack/JSON wire.
///
/// The lossy narrowing to a concrete Rust numeric type happens only at the
/// typed-model boundary (see `terraform-codec`), never inside the value tree.
#[derive(Debug, Clone)]
pub enum Number {
    /// A signed integer that fits `i64`.
    I64(i64),
    /// An unsigned integer larger than `i64::MAX` but within `u64`.
    U64(u64),
    /// A value `f64` can hold exactly enough for real configurations.
    F64(f64),
    /// Canonical decimal text for values outside the fast paths — go-cty's
    /// msgpack/JSON string fallback (e.g. integers beyond `u64`, decimals beyond
    /// `f64` precision). Always normalized (no leading/trailing zeros, no `+`).
    Big(String),
}

impl Number {
    /// Build a number from any signed integer, choosing the tightest arm.
    pub fn from_i128(v: i128) -> Number {
        if let Ok(i) = i64::try_from(v) {
            Number::I64(i)
        } else if let Ok(u) = u64::try_from(v) {
            Number::U64(u)
        } else {
            Number::Big(v.to_string())
        }
    }

    /// Build a number from any unsigned integer, choosing the tightest arm.
    pub fn from_u128(v: u128) -> Number {
        if let Ok(i) = i64::try_from(v) {
            Number::I64(i)
        } else if let Ok(u) = u64::try_from(v) {
            Number::U64(u)
        } else {
            Number::Big(v.to_string())
        }
    }

    /// Build a number from an `f64` (kept as a float; integral floats are
    /// re-encoded as integers only at the wire boundary, matching go-cty).
    pub fn from_f64(v: f64) -> Number {
        Number::F64(v)
    }

    /// Parse a decimal string into the tightest faithful arm, or `None` if it is
    /// not a number. Big integers and decimals beyond `f64` precision are
    /// preserved exactly in [`Number::Big`]; `f64`-exact decimals use
    /// [`Number::F64`].
    pub fn try_parse(s: &str) -> Option<Number> {
        let t = s.trim();
        if t.is_empty() {
            return None;
        }
        if let Ok(i) = t.parse::<i64>() {
            return Some(Number::I64(i));
        }
        if let Ok(u) = t.parse::<u64>() {
            return Some(Number::U64(u));
        }
        // A pure integer literal that overflowed i64/u64: preserve it exactly.
        if is_integer_literal(t) {
            return Some(Number::Big(normalize_decimal(t)));
        }
        // A decimal. Use f64 when it round-trips exactly; otherwise keep the
        // exact text so high-precision decimals are not silently truncated.
        if let Ok(f) = t.parse::<f64>() {
            if f.is_finite() {
                let has_exponent = t.bytes().any(|b| b == b'e' || b == b'E');
                if has_exponent || normalize_decimal(&f.to_string()) == normalize_decimal(t) {
                    return Some(Number::F64(f));
                }
                return Some(Number::Big(normalize_decimal(t)));
            }
        }
        None
    }

    /// Convert to `f64`, possibly losing precision (used at the typed-model
    /// boundary, where the target field is itself an `f64`/`f32`).
    pub fn to_f64_lossy(&self) -> f64 {
        match self {
            Number::I64(i) => *i as f64,
            Number::U64(u) => *u as f64,
            Number::F64(f) => *f,
            Number::Big(s) => s.parse::<f64>().unwrap_or(0.0),
        }
    }

    /// Convert to `i128`, possibly losing precision (used when narrowing into a
    /// signed integer model field).
    pub fn to_i128_lossy(&self) -> i128 {
        match self {
            Number::I64(i) => *i as i128,
            Number::U64(u) => *u as i128,
            Number::F64(f) => *f as i128,
            Number::Big(s) => s
                .parse::<i128>()
                .unwrap_or_else(|_| s.parse::<f64>().map(|f| f as i128).unwrap_or(0)),
        }
    }

    /// Convert to `u128`, possibly losing precision (used when narrowing into an
    /// unsigned integer model field).
    pub fn to_u128_lossy(&self) -> u128 {
        match self {
            Number::I64(i) => (*i).max(0) as u128,
            Number::U64(u) => *u as u128,
            Number::F64(f) => f.max(0.0) as u128,
            Number::Big(s) => s
                .parse::<u128>()
                .unwrap_or_else(|_| s.parse::<f64>().map(|f| f.max(0.0) as u128).unwrap_or(0)),
        }
    }

    /// The exact `i64` value, if this number is an integer that fits.
    pub fn as_i64_exact(&self) -> Option<i64> {
        match self {
            Number::I64(i) => Some(*i),
            Number::U64(_) => None,
            Number::F64(f) => (f.fract() == 0.0 && *f >= i64::MIN as f64 && *f <= i64::MAX as f64)
                .then_some(*f as i64),
            Number::Big(s) => s.parse::<i64>().ok(),
        }
    }

    /// The exact `u64` value, if this number is a non-negative integer that fits.
    pub fn as_u64_exact(&self) -> Option<u64> {
        match self {
            Number::I64(i) => u64::try_from(*i).ok(),
            Number::U64(u) => Some(*u),
            Number::F64(f) => {
                (f.fract() == 0.0 && *f >= 0.0 && *f <= u64::MAX as f64).then_some(*f as u64)
            }
            Number::Big(s) => s.parse::<u64>().ok(),
        }
    }

    /// The canonical decimal text used for value equality and [`fmt::Display`].
    fn canonical(&self) -> String {
        match self {
            Number::I64(i) => i.to_string(),
            Number::U64(u) => u.to_string(),
            Number::F64(f) => f.to_string(),
            Number::Big(s) => s.clone(),
        }
    }
}

impl PartialEq for Number {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Number::I64(a), Number::I64(b)) => a == b,
            (Number::U64(a), Number::U64(b)) => a == b,
            (Number::F64(a), Number::F64(b)) => a == b,
            (Number::Big(a), Number::Big(b)) => a == b,
            // Cross-representation: compare canonical decimal text so that, e.g.,
            // `I64(2)` and `F64(2.0)` are equal.
            _ => self.canonical() == other.canonical(),
        }
    }
}

impl fmt::Display for Number {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Number::I64(i) => write!(f, "{i}"),
            Number::U64(u) => write!(f, "{u}"),
            Number::F64(n) => write!(f, "{n}"),
            Number::Big(s) => f.write_str(s),
        }
    }
}

macro_rules! from_signed {
    ($($t:ty),*) => {$(
        impl From<$t> for Number {
            fn from(v: $t) -> Self { Number::from_i128(v as i128) }
        }
    )*};
}
from_signed!(i8, i16, i32, i64, isize);

macro_rules! from_unsigned {
    ($($t:ty),*) => {$(
        impl From<$t> for Number {
            fn from(v: $t) -> Self { Number::from_u128(v as u128) }
        }
    )*};
}
from_unsigned!(u8, u16, u32, u64, usize);

impl From<f32> for Number {
    fn from(v: f32) -> Self {
        Number::from_f64(v as f64)
    }
}

impl From<f64> for Number {
    fn from(v: f64) -> Self {
        Number::from_f64(v)
    }
}

/// True if `t` is a bare integer literal: an optional sign followed by digits.
fn is_integer_literal(t: &str) -> bool {
    let body = t.strip_prefix(['+', '-']).unwrap_or(t);
    !body.is_empty() && body.bytes().all(|b| b.is_ascii_digit())
}

/// Normalize a decimal string to canonical form: no `+`, no leading zeros in the
/// integer part, no trailing zeros in the fractional part, `-0` collapses to `0`.
fn normalize_decimal(s: &str) -> String {
    let s = s.trim();
    let (neg, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (int_part, frac_part) = match body.split_once('.') {
        Some((i, f)) => (i, f),
        None => (body, ""),
    };
    let int_trim = int_part.trim_start_matches('0');
    let int_norm = if int_trim.is_empty() { "0" } else { int_trim };
    let frac_norm = frac_part.trim_end_matches('0');

    let is_zero = int_norm == "0" && frac_norm.is_empty();
    let mut out = String::new();
    if neg && !is_zero {
        out.push('-');
    }
    out.push_str(int_norm);
    if !frac_norm.is_empty() {
        out.push('.');
        out.push_str(frac_norm);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_integers_use_i64() {
        assert_eq!(Number::from(3_i64), Number::I64(3));
        assert_eq!(Number::try_parse("3"), Some(Number::I64(3)));
        assert_eq!(Number::try_parse("-42"), Some(Number::I64(-42)));
    }

    #[test]
    fn large_64bit_integer_is_exact() {
        // 2^53 + 1: the canonical value f64 cannot represent.
        let n = Number::from(9_007_199_254_740_993_i64);
        assert_eq!(n, Number::I64(9_007_199_254_740_993));
        assert_eq!(n.as_i64_exact(), Some(9_007_199_254_740_993));
        assert_eq!(
            Number::try_parse("9007199254740993"),
            Some(Number::I64(9_007_199_254_740_993))
        );
    }

    #[test]
    fn values_above_i64_use_u64() {
        let big = u64::MAX;
        assert_eq!(Number::from(big), Number::U64(big));
        assert_eq!(Number::try_parse(&big.to_string()), Some(Number::U64(big)));
        assert_eq!(Number::U64(big).as_u64_exact(), Some(big));
    }

    #[test]
    fn integers_beyond_u64_are_preserved_as_text() {
        let s = "170141183460469231731687303715884105729"; // 2^127 + 1
        assert_eq!(Number::try_parse(s), Some(Number::Big(s.to_string())));
        assert_eq!(
            Number::from_u128(u128::MAX),
            Number::Big(u128::MAX.to_string())
        );
    }

    #[test]
    fn exact_decimals_use_f64() {
        assert_eq!(Number::try_parse("3.5"), Some(Number::F64(3.5)));
        assert_eq!(Number::try_parse("2.0"), Some(Number::F64(2.0)));
        assert_eq!(Number::try_parse("1e10"), Some(Number::F64(1e10)));
    }

    #[test]
    fn high_precision_decimals_are_preserved_as_text() {
        let s = "0.12345678901234567890123456789";
        assert_eq!(
            Number::try_parse(s),
            Some(Number::Big("0.12345678901234567890123456789".to_string()))
        );
    }

    #[test]
    fn equality_is_by_value_across_arms() {
        assert_eq!(Number::I64(2), Number::F64(2.0));
        assert_eq!(Number::from(2_u64), Number::I64(2));
        assert_ne!(Number::I64(2), Number::F64(2.5));
        assert_ne!(Number::I64(2), Number::Big("3".to_string()));
    }

    #[test]
    fn rejects_non_numbers() {
        assert_eq!(Number::try_parse(""), None);
        assert_eq!(Number::try_parse("abc"), None);
        assert_eq!(Number::try_parse("NaN"), None);
    }

    #[test]
    fn normalizes_decimal_text() {
        assert_eq!(normalize_decimal("+007"), "7");
        assert_eq!(normalize_decimal("-0"), "0");
        assert_eq!(normalize_decimal("1.2300"), "1.23");
        assert_eq!(normalize_decimal("00.500"), "0.5");
    }

    #[test]
    fn lossy_narrowing_at_boundary() {
        assert_eq!(Number::I64(7).to_i128_lossy(), 7);
        assert_eq!(Number::U64(u64::MAX).to_u128_lossy(), u64::MAX as u128);
        assert_eq!(Number::F64(2.5).to_f64_lossy(), 2.5);
        assert_eq!(
            Number::I64(9_007_199_254_740_993).to_i128_lossy(),
            9_007_199_254_740_993
        );
    }
}
