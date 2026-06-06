use std::collections::BTreeMap;

use crate::number::Number;
use crate::ty::{ObjectAttr, Type};

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
    /// A number.
    ///
    /// Terraform numbers are arbitrary precision. [`Number`] preserves that:
    /// 64-bit integers stay exact (no silent `f64` truncation above 2^53), and
    /// values beyond `i64`/`u64`/`f64` are kept verbatim as canonical decimal
    /// text. The lossy narrowing to a concrete Rust numeric type happens only at
    /// the typed-model boundary, not here.
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

    /// Construct a [`Value::Number`] from anything convertible into a [`Number`]
    /// (any Rust integer or float), e.g. `Value::number(3)` or
    /// `Value::number(1.5)`.
    pub fn number(n: impl Into<Number>) -> Value {
        Value::Number(n.into())
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
}
