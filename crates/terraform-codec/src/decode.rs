//! Decode `cty` msgpack into a [`Value`], directed by its schema [`Type`].

use std::collections::BTreeMap;

use facet_value::Value as Json;
use rmpv::Value as Mp;
use terraform_value::{Type, Value};

use crate::CodecError;

/// Decode `cty` msgpack `bytes` (interpreted as `ty`) into a [`Value`].
///
/// Empty input decodes to [`Value::Null`] (Terraform sends an empty
/// `DynamicValue.msgpack` for a wholly-null value).
pub fn decode_msgpack(bytes: &[u8], ty: &Type) -> Result<Value, CodecError> {
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    let mut cursor = bytes;
    let mp =
        rmpv::decode::read_value(&mut cursor).map_err(|e| CodecError::Decode(e.to_string()))?;
    from_mp(&mp, ty)
}

/// Decode a `cty` **JSON** value (the encoding Terraform uses for stored state,
/// e.g. in `UpgradeResourceState`) into a [`Value`], directed by `ty`.
///
/// JSON state is always wholly known, so there is no unknown handling here.
pub fn decode_json(json: &Json, ty: &Type) -> Result<Value, CodecError> {
    if json.is_null() {
        return Ok(Value::Null);
    }
    match ty {
        Type::Bool => json
            .as_bool()
            .map(Value::Bool)
            .ok_or_else(|| json_mismatch("bool", json)),
        Type::Number => {
            if let Some(n) = json.as_number() {
                Ok(Value::Number(n.to_f64_lossy()))
            } else if let Some(s) = json.as_string() {
                s.as_str()
                    .parse::<f64>()
                    .map(Value::Number)
                    .map_err(|_| json_mismatch("number", json))
            } else {
                Err(json_mismatch("number", json))
            }
        }
        Type::String => json
            .as_string()
            .map(|s| Value::String(s.as_str().to_string()))
            .ok_or_else(|| json_mismatch("string", json)),
        Type::List(elem) => Ok(Value::List(decode_json_each(
            json_array(json, "list")?,
            elem,
        )?)),
        Type::Set(elem) => Ok(Value::Set(decode_json_each(
            json_array(json, "set")?,
            elem,
        )?)),
        Type::Tuple(elems) => {
            let items = json_array(json, "tuple")?;
            if items.len() != elems.len() {
                return Err(CodecError::TypeMismatch {
                    expected: format!("tuple of {} elements", elems.len()),
                    found: "array of different length",
                });
            }
            let values = items
                .iter()
                .zip(elems)
                .map(|(v, t)| decode_json(v, t))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Value::Tuple(values))
        }
        Type::Map(elem) => {
            let obj = json_object(json, "map")?;
            let mut entries = BTreeMap::new();
            for (k, v) in obj.iter() {
                entries.insert(k.as_str().to_string(), decode_json(v, elem)?);
            }
            Ok(Value::Map(entries))
        }
        Type::Object(attrs) => {
            let obj = json_object(json, "object")?;
            let mut fields = BTreeMap::new();
            for attr in attrs {
                let value = match obj.get(&attr.name) {
                    Some(v) => decode_json(v, &attr.ty)?,
                    None => Value::Null,
                };
                fields.insert(attr.name.clone(), value);
            }
            Ok(Value::Object(fields))
        }
        // Best-effort: infer a concrete type from the JSON shape.
        Type::Dynamic => decode_json_dynamic(json),
    }
}

fn decode_json_each(items: &[Json], elem: &Type) -> Result<Vec<Value>, CodecError> {
    items.iter().map(|v| decode_json(v, elem)).collect()
}

fn decode_json_dynamic(json: &Json) -> Result<Value, CodecError> {
    if json.is_null() {
        return Ok(Value::Null);
    }
    if let Some(b) = json.as_bool() {
        return Ok(Value::Bool(b));
    }
    if let Some(n) = json.as_number() {
        return Ok(Value::Number(n.to_f64_lossy()));
    }
    if let Some(s) = json.as_string() {
        return Ok(Value::String(s.as_str().to_string()));
    }
    if let Some(items) = json.as_array() {
        return Ok(Value::Tuple(
            items
                .as_slice()
                .iter()
                .map(decode_json_dynamic)
                .collect::<Result<_, _>>()?,
        ));
    }
    if let Some(obj) = json.as_object() {
        let mut fields = BTreeMap::new();
        for (k, v) in obj.iter() {
            fields.insert(k.as_str().to_string(), decode_json_dynamic(v)?);
        }
        return Ok(Value::Object(fields));
    }
    Err(CodecError::Decode(
        "unsupported dynamic JSON value".to_string(),
    ))
}

fn json_array<'a>(json: &'a Json, expected: &str) -> Result<&'a [Json], CodecError> {
    json.as_array()
        .map(|a| a.as_slice())
        .ok_or_else(|| json_mismatch(expected, json))
}

fn json_object<'a>(json: &'a Json, expected: &str) -> Result<&'a facet_value::VObject, CodecError> {
    json.as_object()
        .ok_or_else(|| json_mismatch(expected, json))
}

fn json_mismatch(expected: &str, json: &Json) -> CodecError {
    let found = if json.is_null() {
        "null"
    } else if json.as_bool().is_some() {
        "bool"
    } else if json.as_number().is_some() {
        "number"
    } else if json.as_string().is_some() {
        "string"
    } else if json.as_array().is_some() {
        "array"
    } else if json.as_object().is_some() {
        "object"
    } else {
        "value"
    };
    CodecError::TypeMismatch {
        expected: expected.to_string(),
        found,
    }
}

/// Convert an [`rmpv::Value`] to a [`Value`] under schema `ty`.
fn from_mp(mp: &Mp, ty: &Type) -> Result<Value, CodecError> {
    // null and unknown are recognized before consulting the type.
    match mp {
        Mp::Nil => return Ok(Value::Null),
        // Any extension is an unknown value; type 12 carries refinements, which
        // we intentionally discard (treating the value as plainly unknown).
        Mp::Ext(_, _) => return Ok(Value::Unknown),
        _ => {}
    }

    match ty {
        Type::Bool => match mp {
            Mp::Boolean(b) => Ok(Value::Bool(*b)),
            other => Err(mismatch("bool", other)),
        },
        Type::Number => number_from_mp(mp)
            .map(Value::Number)
            .ok_or_else(|| mismatch("number", mp)),
        Type::String => mp
            .as_str()
            .map(|s| Value::String(s.to_string()))
            .ok_or_else(|| mismatch("string", mp)),
        Type::List(elem) => Ok(Value::List(decode_each(as_array(mp, "list")?, elem)?)),
        Type::Set(elem) => Ok(Value::Set(decode_each(as_array(mp, "set")?, elem)?)),
        Type::Tuple(elems) => decode_tuple(mp, elems),
        Type::Map(elem) => decode_map(mp, elem),
        Type::Object(attrs) => decode_object(mp, attrs),
        Type::Dynamic => decode_dynamic(mp),
    }
}

/// Decode a homogeneous sequence.
fn decode_each(items: &[Mp], elem: &Type) -> Result<Vec<Value>, CodecError> {
    items.iter().map(|v| from_mp(v, elem)).collect()
}

/// Decode a tuple, checking arity.
fn decode_tuple(mp: &Mp, elems: &[Type]) -> Result<Value, CodecError> {
    let items = as_array(mp, "tuple")?;
    if items.len() != elems.len() {
        return Err(CodecError::TypeMismatch {
            expected: format!("tuple of {} elements", elems.len()),
            found: "array of different length",
        });
    }
    let values = items
        .iter()
        .zip(elems)
        .map(|(v, t)| from_mp(v, t))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Value::Tuple(values))
}

/// Decode a map (msgpack map with arbitrary string keys).
fn decode_map(mp: &Mp, elem: &Type) -> Result<Value, CodecError> {
    let pairs = as_map(mp, "map")?;
    let mut entries = BTreeMap::new();
    for (k, v) in pairs {
        let key = k
            .as_str()
            .ok_or_else(|| CodecError::Decode("map key was not a string".to_string()))?;
        entries.insert(key.to_string(), from_mp(v, elem)?);
    }
    Ok(Value::Map(entries))
}

/// Decode an object using its declared attributes; absent attributes become null.
fn decode_object(mp: &Mp, attrs: &[terraform_value::ObjectAttr]) -> Result<Value, CodecError> {
    let pairs = as_map(mp, "object")?;
    let mut fields = BTreeMap::new();
    for attr in attrs {
        let found = pairs
            .iter()
            .find(|(k, _)| k.as_str() == Some(attr.name.as_str()));
        let value = match found {
            Some((_, v)) => from_mp(v, &attr.ty)?,
            None => Value::Null,
        };
        fields.insert(attr.name.clone(), value);
    }
    Ok(Value::Object(fields))
}

/// Decode a `DynamicPseudoType` slot: `[type-as-JSON, value]`.
fn decode_dynamic(mp: &Mp) -> Result<Value, CodecError> {
    let items = as_array(mp, "dynamic")?;
    if items.len() != 2 {
        return Err(CodecError::Dynamic(format!(
            "expected a 2-element [type, value] array, found {} elements",
            items.len()
        )));
    }
    let type_bytes = match &items[0] {
        Mp::Binary(b) => b.clone(),
        Mp::String(s) => s.as_bytes().to_vec(),
        other => {
            return Err(CodecError::Dynamic(format!(
                "dynamic type must be bytes/string, found {}",
                mp_kind(other)
            )))
        }
    };
    let ty = Type::from_cty_json_bytes(&type_bytes).map_err(CodecError::Dynamic)?;
    from_mp(&items[1], &ty)
}

/// Interpret an rmpv number as `f64`.
fn number_from_mp(mp: &Mp) -> Option<f64> {
    match mp {
        Mp::Integer(i) => i
            .as_i64()
            .map(|x| x as f64)
            .or_else(|| i.as_u64().map(|x| x as f64)),
        Mp::F64(f) => Some(*f),
        Mp::F32(f) => Some(*f as f64),
        // go-cty falls back to a string for numbers that don't fit i64/f64.
        Mp::String(s) => s.as_str().and_then(|x| x.parse::<f64>().ok()),
        _ => None,
    }
}

fn as_array<'a>(mp: &'a Mp, expected: &str) -> Result<&'a [Mp], CodecError> {
    match mp {
        Mp::Array(items) => Ok(items),
        other => Err(mismatch(expected, other)),
    }
}

#[allow(clippy::type_complexity)]
fn as_map<'a>(mp: &'a Mp, expected: &str) -> Result<&'a [(Mp, Mp)], CodecError> {
    match mp {
        Mp::Map(pairs) => Ok(pairs),
        other => Err(mismatch(expected, other)),
    }
}

fn mismatch(expected: &str, mp: &Mp) -> CodecError {
    CodecError::TypeMismatch {
        expected: expected.to_string(),
        found: mp_kind(mp),
    }
}

/// A short description of an rmpv value's kind, for error messages.
fn mp_kind(mp: &Mp) -> &'static str {
    match mp {
        Mp::Nil => "nil",
        Mp::Boolean(_) => "bool",
        Mp::Integer(_) => "integer",
        Mp::F32(_) | Mp::F64(_) => "float",
        Mp::String(_) => "string",
        Mp::Binary(_) => "binary",
        Mp::Array(_) => "array",
        Mp::Map(_) => "map",
        Mp::Ext(_, _) => "ext",
    }
}
