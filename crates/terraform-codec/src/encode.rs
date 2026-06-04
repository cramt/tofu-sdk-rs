//! Encode a [`Value`] to `cty` msgpack, directed by its schema [`Type`].

use std::collections::BTreeMap;

use rmpv::{Integer, Utf8String, Value as Mp};
use terraform_value::{Type, Value};

use crate::CodecError;

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

/// Encode a number as the smallest faithful msgpack form (int if integral).
fn number_to_mp(n: f64) -> Mp {
    if n.is_finite() && n.fract() == 0.0 && n >= i64::MIN as f64 && n <= i64::MAX as f64 {
        Mp::Integer(Integer::from(n as i64))
    } else {
        Mp::F64(n)
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

fn expect_number(v: &Value) -> Result<f64, CodecError> {
    match v {
        Value::Number(n) => Ok(*n),
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
