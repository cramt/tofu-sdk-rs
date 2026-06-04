use serde_json::{json, Value as Json};

/// The Terraform `cty` type system.
///
/// This is the structural type language Terraform uses to describe both schema
/// attribute types and concrete values. It is deliberately backend-agnostic: it
/// is the vocabulary the provider IR speaks, and the Terraform protocol layer
/// later serializes it to the `cty` JSON encoding via [`Type::to_cty_json`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Type {
    /// `cty.Bool`.
    Bool,
    /// `cty.Number`.
    Number,
    /// `cty.String`.
    String,
    /// `cty.DynamicPseudoType` — the type is determined at runtime.
    Dynamic,
    /// `list(element)` — ordered, homogeneous, indexed by number.
    List(Box<Type>),
    /// `set(element)` — unordered, homogeneous, deduplicated by value.
    Set(Box<Type>),
    /// `map(element)` — homogeneous, indexed by string key.
    Map(Box<Type>),
    /// `object({ name = type, ... })` — heterogeneous, fixed string keys.
    ///
    /// Each attribute carries an `optional` flag; optional attributes are
    /// emitted in the trailing optional-attribute list of the `cty` encoding.
    Object(Vec<ObjectAttr>),
    /// `tuple([type, ...])` — heterogeneous, fixed positional elements.
    Tuple(Vec<Type>),
}

/// A single attribute of an [`Type::Object`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ObjectAttr {
    /// Attribute name (the object key).
    pub name: String,
    /// Attribute type.
    pub ty: Type,
    /// Whether the attribute may be omitted by the caller.
    pub optional: bool,
}

impl Type {
    /// Convenience constructor for `list(elem)`.
    pub fn list(elem: Type) -> Type {
        Type::List(Box::new(elem))
    }

    /// Convenience constructor for `set(elem)`.
    pub fn set(elem: Type) -> Type {
        Type::Set(Box::new(elem))
    }

    /// Convenience constructor for `map(elem)`.
    pub fn map(elem: Type) -> Type {
        Type::Map(Box::new(elem))
    }

    /// Serialize to the `cty` JSON type-constraint encoding.
    ///
    /// This is the encoding Terraform expects in `Schema.Attribute.type`
    /// (transmitted as the JSON bytes of a type constraint). Examples:
    ///
    /// - `String` → `"string"`
    /// - `List(String)` → `["list", "string"]`
    /// - `Object({a: string, b?: number})` → `["object", {"a":"string","b":"number"}, ["b"]]`
    pub fn to_cty_json(&self) -> Json {
        match self {
            Type::Bool => json!("bool"),
            Type::Number => json!("number"),
            Type::String => json!("string"),
            Type::Dynamic => json!("dynamic"),
            Type::List(elem) => json!(["list", elem.to_cty_json()]),
            Type::Set(elem) => json!(["set", elem.to_cty_json()]),
            Type::Map(elem) => json!(["map", elem.to_cty_json()]),
            Type::Tuple(elems) => {
                let elems: Vec<Json> = elems.iter().map(Type::to_cty_json).collect();
                json!(["tuple", elems])
            }
            Type::Object(attrs) => {
                let mut fields = serde_json::Map::new();
                let mut optionals: Vec<Json> = Vec::new();
                for attr in attrs {
                    fields.insert(attr.name.clone(), attr.ty.to_cty_json());
                    if attr.optional {
                        optionals.push(json!(attr.name));
                    }
                }
                if optionals.is_empty() {
                    json!(["object", fields])
                } else {
                    json!(["object", fields, optionals])
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitive_encoding() {
        assert_eq!(Type::String.to_cty_json(), json!("string"));
        assert_eq!(Type::Number.to_cty_json(), json!("number"));
        assert_eq!(Type::Bool.to_cty_json(), json!("bool"));
        assert_eq!(Type::Dynamic.to_cty_json(), json!("dynamic"));
    }

    #[test]
    fn collection_encoding() {
        assert_eq!(
            Type::list(Type::String).to_cty_json(),
            json!(["list", "string"])
        );
        assert_eq!(
            Type::map(Type::Number).to_cty_json(),
            json!(["map", "number"])
        );
        assert_eq!(
            Type::set(Type::list(Type::Bool)).to_cty_json(),
            json!(["set", ["list", "bool"]])
        );
    }

    #[test]
    fn object_with_optionals() {
        let ty = Type::Object(vec![
            ObjectAttr {
                name: "a".into(),
                ty: Type::String,
                optional: false,
            },
            ObjectAttr {
                name: "b".into(),
                ty: Type::Number,
                optional: true,
            },
        ]);
        assert_eq!(
            ty.to_cty_json(),
            json!(["object", {"a": "string", "b": "number"}, ["b"]])
        );
    }

    #[test]
    fn object_without_optionals_omits_trailing_list() {
        let ty = Type::Object(vec![ObjectAttr {
            name: "a".into(),
            ty: Type::String,
            optional: false,
        }]);
        assert_eq!(ty.to_cty_json(), json!(["object", {"a": "string"}]));
    }
}
