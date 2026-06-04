//! Typed encode: a Rust value (via facet reflection) -> the dynamic [`Value`]
//! tree.
//!
//! This is the value-level counterpart to `terraform-reflect` (which reflects
//! *types* into the schema IR): here we reflect a concrete value into a [`Value`]
//! so it can be msgpack-encoded for a `DynamicValue` (e.g. the state returned by
//! `ReadResource`/`ApplyResourceChange`).
//!
//! `Option<T>` maps to [`Value::Null`] when `None`. The inverse direction
//! (`Value` -> Rust) is implemented alongside the `Resource` trait in a later
//! phase, where the target Config/State types and their decode cases are
//! concrete.

use std::collections::BTreeMap;

use facet::{Def, Facet, Peek, ScalarType, Type as FType, UserType};
use terraform_value::Value;

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
pub fn to_value<'f, T: Facet<'f>>(value: &'f T) -> Result<Value, TypedError> {
    peek_to_value(Peek::new(value))
}

fn reflect<E: std::fmt::Display>(e: E) -> TypedError {
    TypedError::Reflect(e.to_string())
}

/// Convert a [`Peek`] (a reflected value cursor) into a [`Value`].
fn peek_to_value(peek: Peek<'_, '_>) -> Result<Value, TypedError> {
    let shape = peek.shape();

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

/// Convert a scalar [`Peek`] into a [`Value`].
fn scalar_to_value(peek: &Peek<'_, '_>, scalar: ScalarType) -> Result<Value, TypedError> {
    let value = match scalar {
        ScalarType::Bool => Value::Bool(*peek.get::<bool>().map_err(reflect)?),
        ScalarType::Str | ScalarType::String | ScalarType::CowStr => Value::String(
            peek.as_str()
                .ok_or(TypedError::Unsupported("string"))?
                .to_string(),
        ),
        ScalarType::F64 => Value::Number(*peek.get::<f64>().map_err(reflect)?),
        ScalarType::F32 => Value::Number(*peek.get::<f32>().map_err(reflect)? as f64),
        ScalarType::I8 => Value::Number(*peek.get::<i8>().map_err(reflect)? as f64),
        ScalarType::I16 => Value::Number(*peek.get::<i16>().map_err(reflect)? as f64),
        ScalarType::I32 => Value::Number(*peek.get::<i32>().map_err(reflect)? as f64),
        ScalarType::I64 => Value::Number(*peek.get::<i64>().map_err(reflect)? as f64),
        ScalarType::I128 => Value::Number(*peek.get::<i128>().map_err(reflect)? as f64),
        ScalarType::ISize => Value::Number(*peek.get::<isize>().map_err(reflect)? as f64),
        ScalarType::U8 => Value::Number(*peek.get::<u8>().map_err(reflect)? as f64),
        ScalarType::U16 => Value::Number(*peek.get::<u16>().map_err(reflect)? as f64),
        ScalarType::U32 => Value::Number(*peek.get::<u32>().map_err(reflect)? as f64),
        ScalarType::U64 => Value::Number(*peek.get::<u64>().map_err(reflect)? as f64),
        ScalarType::U128 => Value::Number(*peek.get::<u128>().map_err(reflect)? as f64),
        ScalarType::USize => Value::Number(*peek.get::<usize>().map_err(reflect)? as f64),
        ScalarType::Char => return Err(TypedError::Unsupported("char")),
        ScalarType::Unit => return Err(TypedError::Unsupported("unit")),
        _ => return Err(TypedError::Unsupported("scalar")),
    };
    Ok(value)
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
        assert_eq!(fields["count"], Value::Number(3.0));
        assert_eq!(fields["ratio"], Value::Number(1.5));
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
        assert_eq!(inner["b"], Value::Number(7.0));
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
        assert_eq!(fields["b"], Value::Number(42.0));
    }
}
