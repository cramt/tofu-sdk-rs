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
mod typed;

pub use decode::{decode_json, decode_json_value, decode_msgpack};
pub use encode::{encode_json, encode_msgpack};
pub use typed::{from_value, to_value, TypedError};

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

    use terraform_value::{Number, ObjectAttr, Type, Value};

    use super::*;

    /// Encode then decode, asserting the value survives the round trip.
    fn round_trip(value: Value, ty: &Type) {
        let bytes = encode_msgpack(&value, ty).expect("encode");
        let back = decode_msgpack(&bytes, ty).expect("decode");
        assert_eq!(back, value, "round trip for type {ty:?}");
    }

    #[test]
    fn large_integers_round_trip_without_precision_loss() {
        // 2^53 + 1 is the smallest integer an f64 cannot represent — the old
        // `Value::Number(f64)` silently rounded it to 2^53. It must survive both
        // wire formats exactly now.
        let big = Value::Number(Number::I64(9_007_199_254_740_993));
        let beyond_i64 = Value::Number(Number::U64(u64::MAX));

        for v in [&big, &beyond_i64] {
            // msgpack (schema-directed)
            let bytes = encode_msgpack(v, &Type::Number).expect("encode msgpack");
            assert_eq!(&decode_msgpack(&bytes, &Type::Number).unwrap(), v);

            // cty JSON (dynamic, schema-less decode)
            let json = encode_json(v);
            assert_eq!(&decode_json_value(&json).unwrap(), v);
        }

        // Guard the assertion itself: the rounded-down neighbour is NOT equal,
        // so the round trip above is proving exactness, not numeric mush.
        assert_ne!(big, Value::Number(Number::I64(9_007_199_254_740_992)));
    }

    #[test]
    fn primitives_round_trip() {
        round_trip(Value::Bool(true), &Type::Bool);
        round_trip(Value::from(42.0), &Type::Number);
        round_trip(Value::from(3.5), &Type::Number);
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
            Value::Set(vec![Value::from(1.0), Value::from(2.0)]),
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
            Value::Tuple(vec![Value::String("x".into()), Value::from(1.0)]),
            &Type::Tuple(vec![Type::String, Type::Number]),
        );
    }

    #[test]
    fn json_round_trips_an_object() {
        // `encode_json` -> `facet_json` -> `facet_json` -> `decode_json` (the
        // path the Node binding uses) survives a round trip when typed.
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
                name: "enabled".into(),
                ty: Type::Bool,
                optional: false,
            },
            ObjectAttr {
                name: "tags".into(),
                ty: Type::map(Type::String),
                optional: true,
            },
            ObjectAttr {
                name: "items".into(),
                ty: Type::list(Type::String),
                optional: true,
            },
        ]);
        let mut tags = BTreeMap::new();
        tags.insert("env".to_string(), Value::String("prod".into()));
        let mut obj = BTreeMap::new();
        obj.insert("name".to_string(), Value::String("bucket".into()));
        obj.insert("count".to_string(), Value::from(3.0));
        obj.insert("enabled".to_string(), Value::Bool(true));
        obj.insert("tags".to_string(), Value::Map(tags));
        obj.insert(
            "items".to_string(),
            Value::List(vec![Value::String("a".into())]),
        );
        let value = Value::Object(obj);

        let json = facet_json::to_string(&encode_json(&value)).expect("serialize");
        let parsed: facet_value::Value = facet_json::from_slice(json.as_bytes()).expect("parse");
        let back = decode_json(&parsed, &ty).expect("decode");
        assert_eq!(back, value, "JSON round trip");
    }

    #[test]
    fn json_encodes_unknown_as_null() {
        // JSON cannot carry "unknown"; it degrades to null (decoded as the zero
        // value by the typed layer — the handler is expected to fill computed
        // fields anyway).
        let json = facet_json::to_string(&encode_json(&Value::Unknown)).expect("serialize");
        assert_eq!(json, "null");
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

    #[test]
    fn json_state_decodes() {
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
                name: "tags".into(),
                ty: Type::map(Type::String),
                optional: false,
            },
        ]);
        let json: facet_value::Value =
            facet_json::from_str(r#"{ "name": "bucket", "count": 3, "tags": { "env": "prod" } }"#)
                .expect("parse json state");
        let value = decode_json(&json, &ty).expect("decode json state");
        let Value::Object(fields) = value else {
            panic!("expected object")
        };
        assert_eq!(fields["name"], Value::String("bucket".into()));
        assert_eq!(fields["count"], Value::from(3.0));
        let Value::Map(ref tags) = fields["tags"] else {
            panic!("tags map")
        };
        assert_eq!(tags["env"], Value::String("prod".into()));
    }
}
