//! Typed encode/decode between Rust values (via facet reflection) and the
//! dynamic [`Value`] tree.
//!
//! This is the value-level counterpart to `terraform-reflect` (which reflects
//! *types* into the schema IR):
//!
//! - [`to_value`] reflects a concrete Rust value into a [`Value`] (e.g. the
//!   state returned by `ReadResource`/`ApplyResourceChange`).
//! - [`from_value`] builds a Rust value from a [`Value`] (e.g. decoding the
//!   config/planned state passed to a resource).
//!
//! `Option<T>` maps to [`Value::Null`] when `None`. Because plain Rust types
//! cannot represent Terraform's "unknown", [`from_value`] decodes
//! [`Value::Unknown`] (and [`Value::Null`] for non-`Option` fields) as the
//! type's zero/default — resource handlers fill computed fields in afterwards,
//! so the lost distinction does not matter in practice. (A future `TfValue<T>`
//! field wrapper can preserve it where it does.)

use std::collections::BTreeMap;

use facet::{Def, Facet, Partial, Peek, ScalarType, Shape, Type as FType, UserType};
use terraform_value::{Number, Value};

/// A null constant for defaulting absent struct fields.
const NULL: Value = Value::Null;

/// The type identifier of [`terraform_value::TfValue`], special-cased by the
/// codec to preserve the known/unknown/null distinction.
const TFVALUE: &str = "TfValue";

/// Errors from reflecting a Rust value into a [`Value`].
#[derive(Debug, thiserror::Error)]
pub enum TypedError {
    /// A facet reflection operation failed.
    #[error("reflection error: {0}")]
    Reflect(String),

    /// A map used a non-string key, which `cty` cannot represent.
    #[error("map key was not a string")]
    NonStringKey,

    /// The Rust type has no Terraform value mapping.
    #[error("unsupported type for a terraform value: {0}")]
    Unsupported(&'static str),
}

/// Reflect a Rust value into a [`Value`].
pub fn to_value<T: Facet<'static>>(value: &T) -> Result<Value, TypedError> {
    peek_to_value(Peek::new(value))
}

fn reflect<E: std::fmt::Display>(e: E) -> TypedError {
    TypedError::Reflect(e.to_string())
}

/// Convert a [`Peek`] (a reflected value cursor) into a [`Value`].
fn peek_to_value(peek: Peek<'_, '_>) -> Result<Value, TypedError> {
    let shape = peek.shape();

    // `TfValue<T>` preserves the known/unknown/null trichotomy that plain types
    // collapse: Known(inner) -> the inner value, Unknown -> Value::Unknown,
    // Null -> Value::Null.
    if shape.type_identifier == TFVALUE {
        return tfvalue_to_value(peek);
    }

    // A container-level proxy type (`#[facet(opaque, proxy = P)]`, e.g. a quotient
    // newtype like an ARN or case-insensitive id): serialize through its proxy
    // representation by running `convert_out` (the type's `TryFrom<&T> for P`), then
    // encode the proxy value. This is what lets a quotient type be a real model
    // field rather than only a canonicalizer source (roadmap 3.6).
    if shape.has_any_proxy() {
        if let Some(proxy) = peek.custom_serialization_from_shape().map_err(reflect)? {
            return peek_to_value(proxy.as_peek());
        }
    }

    // Containers and option are recognized by their semantic `Def`.
    match shape.def {
        Def::Option(_) => {
            let opt = peek.into_option().map_err(reflect)?;
            return match opt.value() {
                Some(inner) => peek_to_value(inner),
                None => Ok(Value::Null),
            };
        }
        Def::List(_) | Def::Slice(_) | Def::Array(_) => {
            let list = peek.into_list().map_err(reflect)?;
            let items = list.iter().map(peek_to_value).collect::<Result<_, _>>()?;
            return Ok(Value::List(items));
        }
        Def::Set(_) => {
            let set = peek.into_set().map_err(reflect)?;
            let items = set.iter().map(peek_to_value).collect::<Result<_, _>>()?;
            return Ok(Value::Set(items));
        }
        Def::Map(_) => {
            let map = peek.into_map().map_err(reflect)?;
            let mut entries = BTreeMap::new();
            for (k, v) in map.iter() {
                let key = k.as_str().ok_or(TypedError::NonStringKey)?.to_string();
                entries.insert(key, peek_to_value(v)?);
            }
            return Ok(Value::Map(entries));
        }
        _ => {}
    }

    // Scalars.
    if let Some(scalar) = peek.scalar_type() {
        return scalar_to_value(&peek, scalar);
    }

    // Nested struct -> object.
    match &shape.ty {
        FType::User(UserType::Struct(st)) => {
            let view = peek.into_struct().map_err(reflect)?;
            let mut fields = BTreeMap::new();
            for (index, field) in st.fields.iter().enumerate() {
                let field_peek = view.field(index).map_err(reflect)?;
                let key = field.rename.unwrap_or(field.name).to_string();
                fields.insert(key, peek_to_value(field_peek)?);
            }
            Ok(Value::Object(fields))
        }
        _ => Err(TypedError::Unsupported(shape.type_identifier)),
    }
}

/// Convert a `TfValue<T>` [`Peek`] into a [`Value`], reading its active variant.
fn tfvalue_to_value(peek: Peek<'_, '_>) -> Result<Value, TypedError> {
    let en = peek.into_enum().map_err(reflect)?;
    match en.variant_name_active().map_err(reflect)? {
        "Known" => {
            let inner = en
                .field(0)
                .map_err(reflect)?
                .ok_or(TypedError::Unsupported("TfValue::Known payload"))?;
            peek_to_value(inner)
        }
        "Unknown" => Ok(Value::Unknown),
        _ => Ok(Value::Null),
    }
}

/// Convert a scalar [`Peek`] into a [`Value`].
fn scalar_to_value(peek: &Peek<'_, '_>, scalar: ScalarType) -> Result<Value, TypedError> {
    let value = match scalar {
        ScalarType::Bool => Value::Bool(*peek.get::<bool>().map_err(reflect)?),
        ScalarType::Str | ScalarType::String | ScalarType::CowStr => Value::String(
            peek.as_str()
                .ok_or(TypedError::Unsupported("string"))?
                .to_string(),
        ),
        // Each integer width keeps its exact value (`Value::from` selects the
        // signed/unsigned `Number` case); `i128`/`u128` narrow to `i64`/`u64`
        // when they fit and only fall back to a lossy `f64` past that.
        ScalarType::F64 => Value::from(*peek.get::<f64>().map_err(reflect)?),
        ScalarType::F32 => Value::from(*peek.get::<f32>().map_err(reflect)? as f64),
        ScalarType::I8 => Value::from(*peek.get::<i8>().map_err(reflect)?),
        ScalarType::I16 => Value::from(*peek.get::<i16>().map_err(reflect)?),
        ScalarType::I32 => Value::from(*peek.get::<i32>().map_err(reflect)?),
        ScalarType::I64 => Value::from(*peek.get::<i64>().map_err(reflect)?),
        ScalarType::I128 => wide_int_to_value(*peek.get::<i128>().map_err(reflect)?),
        ScalarType::ISize => Value::from(*peek.get::<isize>().map_err(reflect)?),
        ScalarType::U8 => Value::from(*peek.get::<u8>().map_err(reflect)?),
        ScalarType::U16 => Value::from(*peek.get::<u16>().map_err(reflect)?),
        ScalarType::U32 => Value::from(*peek.get::<u32>().map_err(reflect)?),
        ScalarType::U64 => Value::from(*peek.get::<u64>().map_err(reflect)?),
        ScalarType::U128 => {
            let v = *peek.get::<u128>().map_err(reflect)?;
            u64::try_from(v)
                .map(Value::from)
                .unwrap_or_else(|_| Value::from(v as f64))
        }
        ScalarType::USize => Value::from(*peek.get::<usize>().map_err(reflect)?),
        ScalarType::Char => return Err(TypedError::Unsupported("char")),
        ScalarType::Unit => return Err(TypedError::Unsupported("unit")),
        _ => return Err(TypedError::Unsupported("scalar")),
    };
    Ok(value)
}

/// Build a Rust value of type `T` from a [`Value`].
pub fn from_value<T: Facet<'static>>(value: &Value) -> Result<T, TypedError> {
    let partial = Partial::alloc::<T>().map_err(reflect)?;
    let partial = fill(partial, value)?;
    partial
        .build()
        .map_err(reflect)?
        .materialize::<T>()
        .map_err(reflect)
}

/// Canonicalize a [`Value`] through a quotient type identified at runtime by its
/// [`Shape`] (a field's `#[facet(opaque, proxy = …)]` type, possibly wrapped in
/// `Option`). The value is decoded into `shape` — running the canonicalizing
/// `convert_in` (`TryFrom<Proxy>`) — then re-encoded — running `convert_out`
/// (`TryFrom<&T> for Proxy`). The result is the value's canonical representative.
///
/// This is the **type-erased** counterpart to
/// [`string_quotient`](../terraform_runtime/normalize/fn.string_quotient.html): it
/// needs no static `T`, so a `Canon` can be auto-harvested from a model's
/// `SHAPE` by reflection. On any failure (a value the type rejects, a non-matching
/// shape) the input is returned unchanged — canonicalization is best-effort, never
/// fatal to a plan.
pub fn canonicalize_through_shape(shape: &'static Shape, value: &Value) -> Value {
    canonicalize_try(shape, value).unwrap_or_else(|_| value.clone())
}

// `Partial::alloc_shape` is the one unsafe seam in this crate: it allocates by a
// runtime `Shape` rather than a static `T`. The SAFETY obligation is that the shape
// describes a real, sized type — guaranteed here because every shape reaching this
// function is reflected off a genuine `Facet` model (`field.shape()` of `M::SHAPE`).
#[allow(unsafe_code)]
fn canonicalize_try(shape: &'static Shape, value: &Value) -> Result<Value, TypedError> {
    // SAFETY: `shape` comes from a real `Facet` type's reflected fields
    // (`field.shape()` off `M::SHAPE`), so it describes a genuine, sized type.
    let partial = unsafe { Partial::alloc_shape(shape) }.map_err(reflect)?;
    let partial = fill(partial, value)?;
    let built = partial.build().map_err(reflect)?;
    peek_to_value(built.peek())
}

/// Drive a [`Partial`] builder from a [`Value`], directed by the partial's
/// expected shape at each position.
fn fill<'f, const B: bool>(
    partial: Partial<'f, B>,
    value: &Value,
) -> Result<Partial<'f, B>, TypedError> {
    // `TfValue<T>` decodes preserving the distinction plain types collapse:
    // Unknown -> the `Unknown` variant, Null -> `Null`, anything else -> `Known`.
    if partial.shape().type_identifier == TFVALUE {
        return fill_tfvalue(partial, value);
    }

    // A container-level proxy type (`#[facet(opaque, proxy = P)]`): decode the
    // proxy representation (e.g. a `String`) then let facet run `convert_in` (the
    // type's `TryFrom<P>`), which is the canonicalizing parse for a quotient type.
    // `begin_custom_deserialization_from_shape` pushes a frame for the proxy shape;
    // we recurse to fill it, then `end()` triggers the conversion. If the shape
    // turns out not to be a container proxy, the partial is returned unchanged and
    // we fall through to the normal mapping.
    if partial.shape().has_any_proxy() {
        let (partial, began) = partial
            .begin_custom_deserialization_from_shape()
            .map_err(reflect)?;
        if began {
            let partial = fill(partial, value)?;
            return partial.end().map_err(reflect);
        }
        return fill_def(partial, value);
    }

    fill_def(partial, value)
}

/// Drive a [`Partial`] from a [`Value`] by the partial's structural shape —
/// the body of [`fill`] after the `TfValue`/proxy special-cases are handled.
/// Split out so the proxy path can fall through to it without re-detecting a
/// proxy (which would loop).
fn fill_def<'f, const B: bool>(
    partial: Partial<'f, B>,
    value: &Value,
) -> Result<Partial<'f, B>, TypedError> {
    match &partial.shape().def {
        Def::Option(_) => match value {
            Value::Null | Value::Unknown => partial.set_default().map_err(reflect),
            inner => {
                let partial = partial.begin_some().map_err(reflect)?;
                let partial = fill(partial, inner)?;
                partial.end().map_err(reflect)
            }
        },
        Def::List(_) | Def::Slice(_) | Def::Array(_) => {
            let mut partial = partial.init_list().map_err(reflect)?;
            for item in sequence(value) {
                partial = partial.begin_list_item().map_err(reflect)?;
                partial = fill(partial, item)?;
                partial = partial.end().map_err(reflect)?;
            }
            Ok(partial)
        }
        Def::Set(_) => {
            let mut partial = partial.init_set().map_err(reflect)?;
            for item in sequence(value) {
                partial = partial.begin_set_item().map_err(reflect)?;
                partial = fill(partial, item)?;
                partial = partial.end().map_err(reflect)?;
            }
            Ok(partial)
        }
        Def::Map(_) => {
            let mut partial = partial.init_map().map_err(reflect)?;
            if let Some(entries) = mapping(value) {
                for (key, v) in entries {
                    // A real `Def::Map` (e.g. `HashMap`) is filled with key/value
                    // frame pairs — `begin_key`/`begin_value`. `begin_object_entry`
                    // is reserved for `Def::DynamicValue` objects and errors here.
                    partial = partial.begin_key().map_err(reflect)?;
                    partial = fill(partial, &Value::String(key.clone()))?;
                    partial = partial.end().map_err(reflect)?;
                    partial = partial.begin_value().map_err(reflect)?;
                    partial = fill(partial, v)?;
                    partial = partial.end().map_err(reflect)?;
                }
            }
            Ok(partial)
        }
        _ => match &partial.shape().ty {
            FType::User(UserType::Struct(st)) => fill_struct(partial, st.fields, value),
            _ => set_scalar(partial, value),
        },
    }
}

/// Fill a `TfValue<T>` partial, selecting the variant from the value's
/// known/unknown/null state.
fn fill_tfvalue<'f, const B: bool>(
    partial: Partial<'f, B>,
    value: &Value,
) -> Result<Partial<'f, B>, TypedError> {
    match value {
        Value::Unknown => partial.select_variant_named("Unknown").map_err(reflect),
        Value::Null => partial.select_variant_named("Null").map_err(reflect),
        inner => {
            let partial = partial.select_variant_named("Known").map_err(reflect)?;
            let partial = partial.begin_nth_field(0).map_err(reflect)?;
            let partial = fill(partial, inner)?;
            partial.end().map_err(reflect)
        }
    }
}

/// Fill a struct's fields by index, defaulting absent/null fields.
fn fill_struct<'f, const B: bool>(
    partial: Partial<'f, B>,
    fields: &'static [facet::Field],
    value: &Value,
) -> Result<Partial<'f, B>, TypedError> {
    let object = mapping(value);
    let mut partial = partial;
    for (index, field) in fields.iter().enumerate() {
        let key = field.rename.unwrap_or(field.name);
        let field_value = object.and_then(|m| m.get(key)).unwrap_or(&NULL);
        partial = partial.begin_nth_field(index).map_err(reflect)?;
        partial = fill(partial, field_value)?;
        partial = partial.end().map_err(reflect)?;
    }
    Ok(partial)
}

/// Set a scalar field, coercing [`Value::Null`]/[`Value::Unknown`] to the type's
/// zero value.
fn set_scalar<'f, const B: bool>(
    partial: Partial<'f, B>,
    value: &Value,
) -> Result<Partial<'f, B>, TypedError> {
    let shape = partial.shape();
    if shape.is_type::<String>() {
        partial.set(as_string(value)).map_err(reflect)
    } else if shape.is_type::<bool>() {
        partial.set(as_bool(value)).map_err(reflect)
    } else if shape.is_type::<f64>() {
        partial
            .set(as_number(value).to_f64_lossy())
            .map_err(reflect)
    } else if shape.is_type::<f32>() {
        partial
            .set(as_number(value).to_f64_lossy() as f32)
            .map_err(reflect)
    } else if shape.is_type::<i64>() {
        partial
            .set(as_number(value).to_i64_lossy())
            .map_err(reflect)
    } else if shape.is_type::<i32>() {
        partial
            .set(as_number(value).to_i64_lossy() as i32)
            .map_err(reflect)
    } else if shape.is_type::<i16>() {
        partial
            .set(as_number(value).to_i64_lossy() as i16)
            .map_err(reflect)
    } else if shape.is_type::<i8>() {
        partial
            .set(as_number(value).to_i64_lossy() as i8)
            .map_err(reflect)
    } else if shape.is_type::<isize>() {
        partial
            .set(as_number(value).to_i64_lossy() as isize)
            .map_err(reflect)
    } else if shape.is_type::<u64>() {
        partial
            .set(as_number(value).to_u64_lossy())
            .map_err(reflect)
    } else if shape.is_type::<u32>() {
        partial
            .set(as_number(value).to_u64_lossy() as u32)
            .map_err(reflect)
    } else if shape.is_type::<u16>() {
        partial
            .set(as_number(value).to_u64_lossy() as u16)
            .map_err(reflect)
    } else if shape.is_type::<u8>() {
        partial
            .set(as_number(value).to_u64_lossy() as u8)
            .map_err(reflect)
    } else if shape.is_type::<usize>() {
        partial
            .set(as_number(value).to_u64_lossy() as usize)
            .map_err(reflect)
    } else {
        Err(TypedError::Unsupported(shape.type_identifier))
    }
}

/// Sequence elements of a list/set/tuple value (empty for anything else).
fn sequence(value: &Value) -> &[Value] {
    match value {
        Value::List(items) | Value::Set(items) | Value::Tuple(items) => items,
        _ => &[],
    }
}

/// Map/object entries of a value, if it is one.
fn mapping(value: &Value) -> Option<&BTreeMap<String, Value>> {
    match value {
        Value::Map(entries) | Value::Object(entries) => Some(entries),
        _ => None,
    }
}

fn as_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        _ => String::new(),
    }
}

fn as_bool(value: &Value) -> bool {
    matches!(value, Value::Bool(true))
}

fn as_number(value: &Value) -> Number {
    match value {
        Value::Number(n) => *n,
        _ => Number::I64(0),
    }
}

/// Lower a wide (`i128`) integer into the narrowest lossless [`Number`] case,
/// only falling back to a lossy `f64` when it exceeds both `i64` and `u64`.
fn wide_int_to_value(v: i128) -> Value {
    i64::try_from(v)
        .map(Value::from)
        .or_else(|_| u64::try_from(v).map(Value::from))
        .unwrap_or_else(|_| Value::from(v as f64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use facet::Facet;
    use std::collections::HashMap;
    use terraform_value::Value;

    #[derive(Facet)]
    #[allow(dead_code)]
    struct Inner {
        a: String,
        b: Option<i64>,
    }

    #[derive(Facet)]
    #[allow(dead_code)]
    struct Sample {
        name: String,
        count: i64,
        ratio: f64,
        enabled: bool,
        tags: HashMap<String, String>,
        items: Vec<String>,
        maybe: Option<String>,
        inner: Inner,
    }

    #[test]
    fn reflects_rust_value_into_value_tree() {
        let mut tags = HashMap::new();
        tags.insert("env".to_string(), "prod".to_string());

        let sample = Sample {
            name: "bucket".into(),
            count: 3,
            ratio: 1.5,
            enabled: true,
            tags,
            items: vec!["a".into(), "b".into()],
            maybe: None,
            inner: Inner {
                a: "x".into(),
                b: Some(7),
            },
        };

        let Value::Object(fields) = to_value(&sample).expect("reflects") else {
            panic!("expected object");
        };

        assert_eq!(fields["name"], Value::String("bucket".into()));
        assert_eq!(fields["count"], Value::from(3.0));
        assert_eq!(fields["ratio"], Value::from(1.5));
        assert_eq!(fields["enabled"], Value::Bool(true));
        assert_eq!(
            fields["items"],
            Value::List(vec![Value::String("a".into()), Value::String("b".into())])
        );
        assert_eq!(fields["maybe"], Value::Null, "None -> Null");

        let Value::Map(ref tags) = fields["tags"] else {
            panic!("tags should be a map");
        };
        assert_eq!(tags["env"], Value::String("prod".into()));

        let Value::Object(ref inner) = fields["inner"] else {
            panic!("inner should be an object");
        };
        assert_eq!(inner["a"], Value::String("x".into()));
        assert_eq!(inner["b"], Value::from(7.0));
    }

    #[test]
    fn some_unwraps_to_inner_value() {
        let sample = Inner {
            a: "y".into(),
            b: Some(42),
        };
        let Value::Object(fields) = to_value(&sample).unwrap() else {
            panic!();
        };
        assert_eq!(fields["b"], Value::from(42.0));
    }

    // Decode round-trips: Rust -> Value -> Rust.

    #[derive(Facet, Debug, PartialEq)]
    struct Decoded {
        name: String,
        count: i64,
        ratio: f64,
        enabled: bool,
        items: Vec<String>,
        maybe: Option<String>,
        nope: Option<String>,
    }

    #[derive(Facet, Debug, PartialEq)]
    struct WithMaps {
        tags: std::collections::HashMap<String, String>,
        labels: Option<std::collections::HashMap<String, String>>,
    }

    #[test]
    fn decodes_map_fields_via_key_value_frames() {
        // Regression: a real `Def::Map` must be filled with `begin_key`/
        // `begin_value`, not `begin_object_entry` (which is DynamicValue-only and
        // errors on a `HashMap`). Covers both a bare map and a `Some(map)`.
        let mut tags = BTreeMap::new();
        tags.insert("env".to_string(), Value::String("prod".into()));
        tags.insert("team".to_string(), Value::String("infra".into()));
        let mut labels = BTreeMap::new();
        labels.insert("tier".to_string(), Value::String("gold".into()));

        let mut obj = BTreeMap::new();
        obj.insert("tags".to_string(), Value::Map(tags));
        obj.insert("labels".to_string(), Value::Map(labels));

        let decoded: WithMaps = from_value(&Value::Object(obj)).expect("decode maps");
        assert_eq!(decoded.tags.get("env").map(String::as_str), Some("prod"));
        assert_eq!(decoded.tags.get("team").map(String::as_str), Some("infra"));
        assert_eq!(
            decoded
                .labels
                .as_ref()
                .and_then(|m| m.get("tier"))
                .map(String::as_str),
            Some("gold"),
        );
    }

    #[test]
    fn decodes_absent_map_as_empty_or_none() {
        // A null map field decodes to empty (bare) / None (optional).
        let mut obj = BTreeMap::new();
        obj.insert("tags".to_string(), Value::Null);
        obj.insert("labels".to_string(), Value::Null);
        let decoded: WithMaps = from_value(&Value::Object(obj)).expect("decode");
        assert!(decoded.tags.is_empty());
        assert_eq!(decoded.labels, None);
    }

    #[test]
    fn decodes_value_into_rust() {
        let mut obj = BTreeMap::new();
        obj.insert("name".to_string(), Value::String("bucket".into()));
        obj.insert("count".to_string(), Value::from(3.0));
        obj.insert("ratio".to_string(), Value::from(2.5));
        obj.insert("enabled".to_string(), Value::Bool(true));
        obj.insert(
            "items".to_string(),
            Value::List(vec![Value::String("a".into())]),
        );
        obj.insert("maybe".to_string(), Value::String("here".into()));
        obj.insert("nope".to_string(), Value::Null);

        let decoded: Decoded = from_value(&Value::Object(obj)).expect("decode");
        assert_eq!(
            decoded,
            Decoded {
                name: "bucket".into(),
                count: 3,
                ratio: 2.5,
                enabled: true,
                items: vec!["a".into()],
                maybe: Some("here".into()),
                nope: None,
            }
        );
    }

    #[test]
    fn unknown_decodes_as_zero_value() {
        // A computed field arriving unknown in a planned state decodes to the
        // type's zero; the handler fills it in afterwards.
        let mut obj = BTreeMap::new();
        obj.insert("name".to_string(), Value::String("x".into()));
        obj.insert("count".to_string(), Value::Unknown);
        obj.insert("ratio".to_string(), Value::Unknown);
        obj.insert("enabled".to_string(), Value::Unknown);
        obj.insert("items".to_string(), Value::Unknown);
        obj.insert("maybe".to_string(), Value::Unknown);
        obj.insert("nope".to_string(), Value::Null);

        let decoded: Decoded = from_value(&Value::Object(obj)).expect("decode");
        assert_eq!(decoded.count, 0);
        assert_eq!(decoded.ratio, 0.0);
        assert!(!decoded.enabled);
        assert!(decoded.items.is_empty());
        assert_eq!(decoded.maybe, None);
    }

    #[derive(Facet, Debug, PartialEq)]
    struct WithTfValue {
        name: String,
        token: terraform_value::TfValue<String>,
        size: terraform_value::TfValue<i64>,
    }

    #[test]
    fn tfvalue_preserves_known_unknown_null() {
        use terraform_value::TfValue;

        // Encode: Known -> inner value, Unknown -> Value::Unknown, Null -> Null.
        let v = WithTfValue {
            name: "x".into(),
            token: TfValue::Known("t".into()),
            size: TfValue::Unknown,
        };
        let Value::Object(fields) = to_value(&v).expect("encode") else {
            panic!("expected object");
        };
        assert_eq!(fields["token"], Value::String("t".into()));
        assert_eq!(fields["size"], Value::Unknown);

        // Decode each variant back, preserving the distinction a plain type loses.
        let mut obj = BTreeMap::new();
        obj.insert("name".to_string(), Value::String("x".into()));
        obj.insert("token".to_string(), Value::Null);
        obj.insert("size".to_string(), Value::Unknown);
        let decoded: WithTfValue = from_value(&Value::Object(obj)).expect("decode");
        assert_eq!(
            decoded,
            WithTfValue {
                name: "x".into(),
                token: TfValue::Null,
                size: TfValue::Unknown,
            }
        );
    }

    #[test]
    fn tfvalue_known_round_trips() {
        use terraform_value::TfValue;
        let original = WithTfValue {
            name: "rt".into(),
            token: TfValue::Known("tok".into()),
            size: TfValue::Known(7),
        };
        let value = to_value(&original).unwrap();
        let back: WithTfValue = from_value(&value).unwrap();
        assert_eq!(back, original);
    }

    // A string-backed quotient type: `#[facet(opaque, proxy = String)]` makes facet
    // (de)serialize it through its `TryFrom` conversions. The canonicalizing parse
    // lowercases; the render echoes the stored representative. The codec must drive
    // that proxy vtable so the type works as a real model field.
    #[derive(facet::Facet)]
    #[facet(opaque, proxy = String)]
    struct CiId(String);

    #[allow(clippy::infallible_try_from)]
    impl TryFrom<String> for CiId {
        type Error = std::convert::Infallible;
        fn try_from(s: String) -> Result<Self, Self::Error> {
            Ok(CiId(s.to_lowercase()))
        }
    }
    #[allow(clippy::infallible_try_from)]
    impl TryFrom<&CiId> for String {
        type Error = std::convert::Infallible;
        fn try_from(id: &CiId) -> Result<Self, Self::Error> {
            Ok(id.0.clone())
        }
    }

    #[derive(Facet)]
    struct WithQuotient {
        name: String,
        id: CiId,
    }

    #[test]
    fn decode_drives_proxy_convert_in_and_canonicalizes() {
        // The wire carries the user's casing; `convert_in` (TryFrom<String>) runs on
        // decode and canonicalizes to lowercase. Re-encoding through `convert_out`
        // proves the stored representative is the canonical form.
        let mut obj = BTreeMap::new();
        obj.insert("name".to_string(), Value::String("x".into()));
        obj.insert("id".to_string(), Value::String("AbC".into()));

        let decoded: WithQuotient = from_value(&Value::Object(obj)).expect("decode proxy");

        let Value::Object(fields) = to_value(&decoded).expect("encode proxy") else {
            panic!("expected object");
        };
        assert_eq!(
            fields["id"],
            Value::String("abc".into()),
            "convert_in canonicalized on decode; convert_out rendered it back"
        );
        assert_eq!(fields["name"], Value::String("x".into()));
    }

    #[test]
    fn encode_drives_proxy_convert_out() {
        // A value already canonical in Rust encodes to its proxy string.
        let v = WithQuotient {
            name: "n".into(),
            id: CiId::try_from("already-lower".to_string()).unwrap(),
        };
        let Value::Object(fields) = to_value(&v).expect("encode") else {
            panic!("expected object");
        };
        assert_eq!(fields["id"], Value::String("already-lower".into()));
    }

    #[test]
    fn proxy_field_round_trips_through_codec() {
        // Value -> Rust -> Value is stable once canonical (the integration the
        // semantic-equality work was blocked on).
        let mut obj = BTreeMap::new();
        obj.insert("name".to_string(), Value::String("r".into()));
        obj.insert("id".to_string(), Value::String("MixedCase".into()));
        let decoded: WithQuotient = from_value(&Value::Object(obj)).expect("decode");
        let encoded = to_value(&decoded).expect("encode");
        let twice: WithQuotient = from_value(&encoded).expect("decode again");
        let encoded_twice = to_value(&twice).expect("encode again");
        assert_eq!(encoded, encoded_twice);
        let Value::Object(fields) = encoded else {
            panic!("expected object");
        };
        assert_eq!(fields["id"], Value::String("mixedcase".into()));
    }

    #[test]
    fn canonicalize_through_shape_uses_the_proxy_conversions() {
        // The type-erased canonicalizer (no static T): decode-then-encode through
        // the quotient shape lowercases, and is idempotent on an already-canonical
        // value. This is what lets a `Canon` be harvested from a model's SHAPE.
        let shape = <CiId as Facet>::SHAPE;
        assert_eq!(
            canonicalize_through_shape(shape, &Value::String("MiXeD".into())),
            Value::String("mixed".into())
        );
        assert_eq!(
            canonicalize_through_shape(shape, &Value::String("flat".into())),
            Value::String("flat".into())
        );
    }

    #[test]
    fn canonicalize_through_optional_shape_handles_null_and_value() {
        // `Option<Quotient>` composes: a string canonicalizes, null stays null.
        let shape = <Option<CiId> as Facet>::SHAPE;
        assert_eq!(
            canonicalize_through_shape(shape, &Value::String("AbC".into())),
            Value::String("abc".into())
        );
        assert_eq!(canonicalize_through_shape(shape, &Value::Null), Value::Null);
    }

    #[test]
    fn encode_then_decode_round_trips() {
        let original = Decoded {
            name: "rt".into(),
            count: 9,
            ratio: 1.25,
            enabled: false,
            items: vec!["p".into(), "q".into()],
            maybe: Some("m".into()),
            nope: None,
        };
        let value = to_value(&original).unwrap();
        let back: Decoded = from_value(&value).unwrap();
        assert_eq!(back, original);
    }
}
