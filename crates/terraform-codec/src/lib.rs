//! `DynamicValue` codec: Terraform `cty` msgpack <-> the semantic [`Value`] tree.
//!
//! Terraform transmits values in a `DynamicValue` whose `msgpack` field holds a
//! type-directed `cty` msgpack encoding. This crate converts those bytes to and
//! from [`terraform_value::Value`]. The encoding rules follow `go-cty`:
//!
//! - **null** -> msgpack `nil`
//! - **unknown** -> a msgpack extension (`fixext1`, type `0`, body `0x00`); the
//!   decoder accepts any extension as unknown (type `12` carries refinements,
//!   which we read as plain unknown)
//! - **bool/string** -> native; **number** -> int if integral, else float
//! - **list/set/tuple** -> msgpack array; **map/object** -> msgpack map
//! - **DynamicPseudoType** -> a 2-element array `[type-as-JSON, value]`
//!
//! Encoding and decoding are *type-directed*: the schema [`Type`] tells the codec
//! how to interpret each node (e.g. msgpack maps are ambiguous between `map` and
//! `object`).

mod decode;
mod encode;

pub use decode::decode_msgpack;
pub use encode::encode_msgpack;

/// An error from encoding or decoding a `DynamicValue`.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    /// The msgpack bytes could not be parsed.
    #[error("msgpack decode error: {0}")]
    Decode(String),

    /// The value could not be serialized to msgpack.
    #[error("msgpack encode error: {0}")]
    Encode(String),

    /// A value did not match its schema type.
    #[error("type mismatch: expected {expected}, found incompatible {found} value")]
    TypeMismatch {
        /// The expected schema type.
        expected: String,
        /// A short description of the value that was found.
        found: &'static str,
    },

    /// A `DynamicPseudoType` value was malformed.
    #[error("invalid dynamic value: {0}")]
    Dynamic(String),
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use terraform_value::{ObjectAttr, Type, Value};

    use super::*;

    /// Encode then decode, asserting the value survives the round trip.
    fn round_trip(value: Value, ty: &Type) {
        let bytes = encode_msgpack(&value, ty).expect("encode");
        let back = decode_msgpack(&bytes, ty).expect("decode");
        assert_eq!(back, value, "round trip for type {ty:?}");
    }

    #[test]
    fn primitives_round_trip() {
        round_trip(Value::Bool(true), &Type::Bool);
        round_trip(Value::Number(42.0), &Type::Number);
        round_trip(Value::Number(3.5), &Type::Number);
        round_trip(Value::String("hello".into()), &Type::String);
    }

    #[test]
    fn null_and_unknown_round_trip() {
        round_trip(Value::Null, &Type::String);
        round_trip(Value::Unknown, &Type::String);
        round_trip(Value::Null, &Type::Number);
        round_trip(Value::Unknown, &Type::list(Type::String));
    }

    #[test]
    fn collections_round_trip() {
        round_trip(
            Value::List(vec![Value::String("a".into()), Value::String("b".into())]),
            &Type::list(Type::String),
        );
        round_trip(
            Value::Set(vec![Value::Number(1.0), Value::Number(2.0)]),
            &Type::set(Type::Number),
        );
        let mut m = BTreeMap::new();
        m.insert("k".to_string(), Value::String("v".into()));
        round_trip(Value::Map(m), &Type::map(Type::String));
    }

    #[test]
    fn object_round_trips_with_unknown_and_null_fields() {
        let ty = Type::Object(vec![
            ObjectAttr {
                name: "name".into(),
                ty: Type::String,
                optional: false,
            },
            ObjectAttr {
                name: "count".into(),
                ty: Type::Number,
                optional: false,
            },
            ObjectAttr {
                name: "arn".into(),
                ty: Type::String,
                optional: false,
            },
        ]);
        let mut o = BTreeMap::new();
        o.insert("name".to_string(), Value::String("bucket".into()));
        o.insert("count".to_string(), Value::Null);
        o.insert("arn".to_string(), Value::Unknown);
        round_trip(Value::Object(o), &ty);
    }

    #[test]
    fn tuple_round_trips() {
        round_trip(
            Value::Tuple(vec![Value::String("x".into()), Value::Number(1.0)]),
            &Type::Tuple(vec![Type::String, Type::Number]),
        );
    }

    #[test]
    fn dynamic_round_trips() {
        round_trip(Value::String("dyn".into()), &Type::Dynamic);
        round_trip(Value::List(vec![Value::Bool(true)]), &Type::Dynamic);
    }

    #[test]
    fn unknown_uses_cty_ext_bytes() {
        // go-cty encodes a plain unknown as fixext1, type 0, body 0x00.
        let bytes = encode_msgpack(&Value::Unknown, &Type::String).unwrap();
        assert_eq!(bytes, vec![0xd4, 0x00, 0x00]);
    }

    #[test]
    fn null_uses_msgpack_nil() {
        let bytes = encode_msgpack(&Value::Null, &Type::String).unwrap();
        assert_eq!(bytes, vec![0xc0]);
    }

    #[test]
    fn empty_bytes_decode_as_null() {
        assert_eq!(decode_msgpack(&[], &Type::String).unwrap(), Value::Null);
    }

    #[test]
    fn type_mismatch_is_reported() {
        let err = encode_msgpack(&Value::String("x".into()), &Type::Bool).unwrap_err();
        assert!(matches!(err, CodecError::TypeMismatch { .. }));
    }
}
