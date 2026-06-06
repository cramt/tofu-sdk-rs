//! Encode a [`Value`] to `cty` msgpack, directed by its schema [`Type`].

use std::collections::BTreeMap;

use facet_value::{VArray, VObject, Value as Json};
use rmpv::{Integer, Utf8String, Value as Mp};
use terraform_value::{Number, Type, Value};

use crate::CodecError;

/// Encode a [`Value`] into a dynamic `cty` **JSON** value (the inverse of
/// [`decode_json`](crate::decode_json), and unlike msgpack it needs no schema).
///
/// `Unknown` maps to JSON `null` (JSON cannot represent unknown); `Set`/`Tuple`
/// collapse to arrays and `Map`/`Object` to objects. Round-tripping back through
/// `decode_json` with the attribute's [`Type`] reconstructs the precise variant.
pub fn encode_json(value: &Value) -> Json {
    match value {
        Value::Null | Value::Unknown => Json::NULL,
        Value::Bool(b) => (*b).into(),
        Value::Number(n) => number_to_json(n),
        Value::String(s) => s.clone().into(),
        Value::List(items) | Value::Set(items) | Value::Tuple(items) => {
            let mut arr = VArray::with_capacity(items.len());
            for item in items {
                arr.push(encode_json(item));
            }
            arr.into()
        }
        Value::Map(entries) | Value::Object(entries) => {
            let mut obj = VObject::with_capacity(entries.len());
            for (key, val) in entries {
                obj.insert(key.clone(), encode_json(val));
            }
            obj.into()
        }
    }
}

/// Encode `value` (interpreted as `ty`) to `cty` msgpack bytes.
pub fn encode_msgpack(value: &Value, ty: &Type) -> Result<Vec<u8>, CodecError> {
    let mp = to_mp(value, ty)?;
    let mut buf = Vec::new();
    rmpv::encode::write_value(&mut buf, &mp).map_err(|e| CodecError::Encode(e.to_string()))?;
    Ok(buf)
}

/// Convert a [`Value`] to an [`rmpv::Value`] under schema `ty`.
fn to_mp(value: &Value, ty: &Type) -> Result<Mp, CodecError> {
    // null and unknown are encoded uniformly for any type.
    match value {
        Value::Null => return Ok(Mp::Nil),
        // fixext1, ext type 0, body 0x00 — go-cty's plain-unknown marker.
        Value::Unknown => return Ok(Mp::Ext(0, vec![0])),
        _ => {}
    }

    match ty {
        Type::Bool => Ok(Mp::Boolean(expect_bool(value)?)),
        Type::Number => Ok(number_to_mp(expect_number(value)?)),
        Type::String => Ok(Mp::String(Utf8String::from(expect_string(value)?))),
        Type::List(elem) => Ok(Mp::Array(encode_each(expect_list(value)?, elem)?)),
        Type::Set(elem) => Ok(Mp::Array(encode_each(expect_set(value)?, elem)?)),
        Type::Tuple(elems) => encode_tuple(value, elems),
        Type::Map(elem) => encode_map(expect_map(value)?, elem),
        Type::Object(attrs) => encode_object(expect_object(value)?, attrs),
        Type::Dynamic => encode_dynamic(value),
    }
}

/// Encode a homogeneous sequence.
fn encode_each(items: &[Value], elem: &Type) -> Result<Vec<Mp>, CodecError> {
    items.iter().map(|v| to_mp(v, elem)).collect()
}

/// Encode a tuple, checking arity.
fn encode_tuple(value: &Value, elems: &[Type]) -> Result<Mp, CodecError> {
    let items = expect_tuple(value)?;
    if items.len() != elems.len() {
        return Err(CodecError::TypeMismatch {
            expected: format!("tuple of {} elements", elems.len()),
            found: "tuple of different arity",
        });
    }
    let mp = items
        .iter()
        .zip(elems)
        .map(|(v, t)| to_mp(v, t))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Mp::Array(mp))
}

/// Encode a map (msgpack map; keys are the map's own string keys).
fn encode_map(entries: &BTreeMap<String, Value>, elem: &Type) -> Result<Mp, CodecError> {
    let pairs = entries
        .iter()
        .map(|(k, v)| Ok((Mp::String(Utf8String::from(k.as_str())), to_mp(v, elem)?)))
        .collect::<Result<Vec<_>, CodecError>>()?;
    Ok(Mp::Map(pairs))
}

/// Encode an object (msgpack map; keys are the declared attribute names, sorted
/// alphabetically to match go-cty). Missing attributes encode as null.
fn encode_object(
    fields: &BTreeMap<String, Value>,
    attrs: &[terraform_value::ObjectAttr],
) -> Result<Mp, CodecError> {
    let mut sorted: Vec<&terraform_value::ObjectAttr> = attrs.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let pairs = sorted
        .into_iter()
        .map(|attr| {
            let field = fields.get(&attr.name).unwrap_or(&Value::Null);
            Ok((
                Mp::String(Utf8String::from(attr.name.as_str())),
                to_mp(field, &attr.ty)?,
            ))
        })
        .collect::<Result<Vec<_>, CodecError>>()?;
    Ok(Mp::Map(pairs))
}

/// Encode a `DynamicPseudoType` slot: a 2-element array of the concrete type
/// (as cty JSON bytes) and the value encoded with that type.
fn encode_dynamic(value: &Value) -> Result<Mp, CodecError> {
    let concrete = value.infer_type();
    let type_json = concrete.to_cty_json_bytes();
    let inner = to_mp(value, &concrete)?;
    Ok(Mp::Array(vec![Mp::Binary(type_json), inner]))
}

/// Encode a number as the smallest faithful msgpack form, matching go-cty:
/// an `int64`-exact value is an integer, a `float64`-exact value a float, and
/// anything outside (very large integers, high-precision decimals) a string.
fn number_to_mp(n: &Number) -> Mp {
    match n {
        Number::I64(i) => Mp::Integer(Integer::from(*i)),
        Number::U64(u) => Mp::Integer(Integer::from(*u)),
        Number::F64(f) => {
            if f.is_finite() && f.fract() == 0.0 && *f >= i64::MIN as f64 && *f <= i64::MAX as f64 {
                Mp::Integer(Integer::from(*f as i64))
            } else {
                Mp::F64(*f)
            }
        }
        Number::Big(s) => Mp::String(Utf8String::from(s.as_str())),
    }
}

/// Encode a number into a `cty` JSON value. `facet-value` numbers only hold
/// `i64`/`u64`/`f64`, so a [`Number::Big`] value (beyond `u64`) falls back to a
/// lossy `f64` here; the msgpack wire path preserves it exactly. This only
/// affects the JSON paths (state-upgrade reads and the Node binding, where JS
/// numbers are `f64` regardless).
fn number_to_json(n: &Number) -> Json {
    if let Some(i) = n.as_i64_exact() {
        i.into()
    } else if let Some(u) = n.as_u64_exact() {
        u.into()
    } else {
        n.to_f64_lossy().into()
    }
}

// --- typed extractors, producing TypeMismatch on the wrong variant ----------

fn mismatch(expected: &str, found: &Value) -> CodecError {
    CodecError::TypeMismatch {
        expected: expected.to_string(),
        found: kind(found),
    }
}

fn expect_bool(v: &Value) -> Result<bool, CodecError> {
    match v {
        Value::Bool(b) => Ok(*b),
        other => Err(mismatch("bool", other)),
    }
}

fn expect_number(v: &Value) -> Result<&Number, CodecError> {
    match v {
        Value::Number(n) => Ok(n),
        other => Err(mismatch("number", other)),
    }
}

fn expect_string(v: &Value) -> Result<&str, CodecError> {
    match v {
        Value::String(s) => Ok(s),
        other => Err(mismatch("string", other)),
    }
}

fn expect_list(v: &Value) -> Result<&[Value], CodecError> {
    match v {
        Value::List(items) => Ok(items),
        other => Err(mismatch("list", other)),
    }
}

fn expect_set(v: &Value) -> Result<&[Value], CodecError> {
    match v {
        Value::Set(items) => Ok(items),
        other => Err(mismatch("set", other)),
    }
}

fn expect_tuple(v: &Value) -> Result<&[Value], CodecError> {
    match v {
        Value::Tuple(items) => Ok(items),
        other => Err(mismatch("tuple", other)),
    }
}

fn expect_map(v: &Value) -> Result<&BTreeMap<String, Value>, CodecError> {
    match v {
        Value::Map(entries) => Ok(entries),
        other => Err(mismatch("map", other)),
    }
}

fn expect_object(v: &Value) -> Result<&BTreeMap<String, Value>, CodecError> {
    match v {
        Value::Object(fields) => Ok(fields),
        other => Err(mismatch("object", other)),
    }
}

/// A short description of a value's kind, for error messages.
fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Unknown => "unknown",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::List(_) => "list",
        Value::Set(_) => "set",
        Value::Map(_) => "map",
        Value::Object(_) => "object",
        Value::Tuple(_) => "tuple",
    }
}
