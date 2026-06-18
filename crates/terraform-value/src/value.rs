use std::collections::BTreeMap;
use std::str::FromStr;

use crate::ty::{ObjectAttr, Type};

/// A Terraform (`cty`) number.
///
/// cty numbers are arbitrary precision (`big.Float`). This representation keeps a
/// signed integer, an unsigned integer, and a float as distinct cases so the
/// **full 64-bit integer range round-trips losslessly** — real providers carry
/// 64-bit IDs and large byte counts that a single `f64` (53-bit mantissa) would
/// silently truncate. Values outside `i64`/`u64`/`f64` (truly arbitrary
/// precision) are not representable; that matches the limit of the JSON value
/// layer and is acceptable for real configurations.
///
/// Equality is by mathematical value, not by case: `I64(3) == U64(3) ==
/// F64(3.0)`. Two integral numbers compare exactly (via `i128`), so a 64-bit
/// integer is never conflated with a nearby one through an `f64` round-trip.
#[derive(Debug, Clone, Copy)]
pub enum Number {
    /// A signed integer (the default case for integral values that fit `i64`).
    I64(i64),
    /// An unsigned integer beyond `i64::MAX`.
    U64(u64),
    /// A floating-point or otherwise non-integral value.
    F64(f64),
}

impl Number {
    /// The exact integer value if this number is integral and fits an `i128`,
    /// used to compare integers without going through a lossy `f64`.
    fn as_integral(self) -> Option<i128> {
        match self {
            Number::I64(i) => Some(i as i128),
            Number::U64(u) => Some(u as i128),
            Number::F64(f)
                if f.is_finite()
                    && f.fract() == 0.0
                    && f >= i128::MIN as f64
                    && f <= i128::MAX as f64 =>
            {
                Some(f as i128)
            }
            Number::F64(_) => None,
        }
    }

    /// Convert to `f64`, losing precision for integers beyond 2^53.
    pub fn to_f64_lossy(self) -> f64 {
        match self {
            Number::I64(i) => i as f64,
            Number::U64(u) => u as f64,
            Number::F64(f) => f,
        }
    }

    /// Convert to `i64` (saturating for out-of-range floats, wrapping `u64`).
    pub fn to_i64_lossy(self) -> i64 {
        match self {
            Number::I64(i) => i,
            Number::U64(u) => u as i64,
            Number::F64(f) => f as i64,
        }
    }

    /// Convert to `u64` (saturating for out-of-range floats, wrapping `i64`).
    pub fn to_u64_lossy(self) -> u64 {
        match self {
            Number::I64(i) => i as u64,
            Number::U64(u) => u,
            Number::F64(f) => f as u64,
        }
    }
}

impl PartialEq for Number {
    fn eq(&self, other: &Self) -> bool {
        match (self.as_integral(), other.as_integral()) {
            (Some(a), Some(b)) => a == b,
            _ => self.to_f64_lossy() == other.to_f64_lossy(),
        }
    }
}

impl FromStr for Number {
    type Err = std::num::ParseFloatError;

    /// Parse the narrowest faithful case: `i64`, else `u64` (large positive),
    /// else `f64`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Ok(i) = s.parse::<i64>() {
            Ok(Number::I64(i))
        } else if let Ok(u) = s.parse::<u64>() {
            Ok(Number::U64(u))
        } else {
            s.parse::<f64>().map(Number::F64)
        }
    }
}

macro_rules! number_from_int {
    ($($t:ty => $case:ident),* $(,)?) => {$(
        impl From<$t> for Number {
            fn from(v: $t) -> Self {
                Number::$case(v as _)
            }
        }
        impl From<$t> for Value {
            fn from(v: $t) -> Self {
                Value::Number(Number::from(v))
            }
        }
    )*};
}

// Signed and small unsigned integers fit `i64`; `u64`/`usize` need the unsigned
// case to stay lossless above `i64::MAX`.
number_from_int!(
    i8 => I64, i16 => I64, i32 => I64, i64 => I64, isize => I64,
    u8 => I64, u16 => I64, u32 => I64, u64 => U64, usize => U64,
);

impl From<f64> for Number {
    fn from(v: f64) -> Self {
        Number::F64(v)
    }
}

impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Number(Number::F64(v))
    }
}

/// A dynamic Terraform value tree.
///
/// Unlike [`crate::TfValue`] (which wraps a *typed* Rust value), this is the
/// schema-agnostic shape values take on the wire: any node may be [`Value::Null`]
/// or [`Value::Unknown`], and containers nest other [`Value`]s. It is the
/// intermediate representation the codec produces from a `DynamicValue` before a
/// typed decode into Rust structs.
///
/// `Null` and `Unknown` are distinct, mirroring Terraform's semantics (see
/// [`crate::TfValue`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// The value is definitively absent.
    Null,
    /// The value is not yet known (resolved during apply).
    Unknown,
    /// A boolean.
    Bool(bool),
    /// A number — see [`Number`] for the precision guarantees.
    Number(Number),
    /// A string.
    String(String),
    /// An ordered, homogeneous list.
    List(Vec<Value>),
    /// An unordered, homogeneous set (order here is incidental).
    Set(Vec<Value>),
    /// A string-keyed, homogeneous map.
    Map(BTreeMap<String, Value>),
    /// A string-keyed, heterogeneous object with fixed attributes.
    Object(BTreeMap<String, Value>),
    /// A fixed-length, heterogeneous tuple.
    Tuple(Vec<Value>),
}

impl Value {
    /// Returns `true` if this node is [`Value::Null`].
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Returns `true` if this node is [`Value::Unknown`].
    pub fn is_unknown(&self) -> bool {
        matches!(self, Value::Unknown)
    }

    /// Best-effort inference of a concrete [`Type`] from this value.
    ///
    /// Used when encoding a value into a `DynamicPseudoType` schema slot, where
    /// the concrete type must be transmitted alongside the value. `Null`/
    /// `Unknown` and empty containers carry no type information, so they infer to
    /// [`Type::Dynamic`].
    pub fn infer_type(&self) -> Type {
        match self {
            Value::Null | Value::Unknown => Type::Dynamic,
            Value::Bool(_) => Type::Bool,
            Value::Number(_) => Type::Number,
            Value::String(_) => Type::String,
            Value::List(items) => Type::list(
                items
                    .first()
                    .map(Value::infer_type)
                    .unwrap_or(Type::Dynamic),
            ),
            Value::Set(items) => Type::set(
                items
                    .first()
                    .map(Value::infer_type)
                    .unwrap_or(Type::Dynamic),
            ),
            Value::Map(entries) => Type::map(
                entries
                    .values()
                    .next()
                    .map(Value::infer_type)
                    .unwrap_or(Type::Dynamic),
            ),
            Value::Tuple(items) => Type::Tuple(items.iter().map(Value::infer_type).collect()),
            Value::Object(attrs) => Type::Object(
                attrs
                    .iter()
                    .map(|(name, v)| ObjectAttr {
                        name: name.clone(),
                        ty: v.infer_type(),
                        optional: false,
                    })
                    .collect(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_and_unknown_are_distinct() {
        assert_ne!(Value::Null, Value::Unknown);
        assert!(Value::Null.is_null());
        assert!(Value::Unknown.is_unknown());
    }

    #[test]
    fn infers_concrete_types() {
        assert_eq!(Value::Bool(true).infer_type(), Type::Bool);
        assert_eq!(
            Value::List(vec![Value::String("a".into())]).infer_type(),
            Type::list(Type::String)
        );
        assert_eq!(Value::List(vec![]).infer_type(), Type::list(Type::Dynamic));
    }

    #[test]
    fn number_equality_is_by_value_across_cases() {
        assert_eq!(Number::I64(3), Number::U64(3));
        assert_eq!(Number::I64(3), Number::F64(3.0));
        assert_eq!(Number::U64(3), Number::F64(3.0));
        assert_ne!(Number::F64(3.5), Number::I64(3));
    }

    #[test]
    fn large_integers_compare_exactly_not_via_f64() {
        // Two consecutive 64-bit integers that collapse to the same f64; they
        // must stay distinct (the whole point of the int cases).
        let a = Number::I64(9_007_199_254_740_993); // 2^53 + 1
        let b = Number::I64(9_007_199_254_740_992); // 2^53
        assert_ne!(a, b);
        assert_eq!(a.to_f64_lossy(), b.to_f64_lossy()); // ...yet equal as f64
    }

    #[test]
    fn parses_narrowest_faithful_case() {
        assert!(matches!("42".parse::<Number>(), Ok(Number::I64(42))));
        assert!(matches!(
            "18446744073709551615".parse::<Number>(), // u64::MAX
            Ok(Number::U64(u64::MAX))
        ));
        assert!(matches!("3.5".parse::<Number>(), Ok(Number::F64(_))));
    }
}
