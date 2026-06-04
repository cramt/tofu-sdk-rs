use facet_value::{VArray, VObject, Value as Json};

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

    /// Serialize to the `cty` JSON type-constraint encoding (as a dynamic value).
    ///
    /// This is the encoding Terraform expects in `Schema.Attribute.type`
    /// (transmitted as the JSON bytes of a type constraint). Examples:
    ///
    /// - `String` → `"string"`
    /// - `List(String)` → `["list", "string"]`
    /// - `Object({a: string, b?: number})` → `["object", {"a":"string","b":"number"}, ["b"]]`
    pub fn to_cty_json(&self) -> Json {
        match self {
            Type::Bool => Json::from("bool"),
            Type::Number => Json::from("number"),
            Type::String => Json::from("string"),
            Type::Dynamic => Json::from("dynamic"),
            Type::List(elem) => json_array([Json::from("list"), elem.to_cty_json()]),
            Type::Set(elem) => json_array([Json::from("set"), elem.to_cty_json()]),
            Type::Map(elem) => json_array([Json::from("map"), elem.to_cty_json()]),
            Type::Tuple(elems) => {
                let inner = json_array(elems.iter().map(Type::to_cty_json));
                json_array([Json::from("tuple"), inner])
            }
            Type::Object(attrs) => {
                let mut fields = VObject::new();
                let mut optionals: Vec<Json> = Vec::new();
                for attr in attrs {
                    fields.insert(attr.name.as_str(), attr.ty.to_cty_json());
                    if attr.optional {
                        optionals.push(Json::from(attr.name.as_str()));
                    }
                }
                if optionals.is_empty() {
                    json_array([Json::from("object"), Json::from(fields)])
                } else {
                    json_array([
                        Json::from("object"),
                        Json::from(fields),
                        json_array(optionals),
                    ])
                }
            }
        }
    }

    /// Serialize to the `cty` JSON type-constraint encoding as bytes (the form
    /// Terraform carries in `Schema.Attribute.type`).
    pub fn to_cty_json_bytes(&self) -> Vec<u8> {
        facet_json::to_vec(&self.to_cty_json()).expect("a cty type constraint always serializes")
    }

    /// Parse a `cty` JSON type constraint (the inverse of [`Type::to_cty_json`]).
    ///
    /// Used when decoding a `DynamicPseudoType` value, whose wire form carries
    /// the concrete type as embedded JSON.
    pub fn from_cty_json(json: &Json) -> Result<Type, String> {
        if let Some(s) = json.as_string() {
            return match s.as_str() {
                "bool" => Ok(Type::Bool),
                "number" => Ok(Type::Number),
                "string" => Ok(Type::String),
                "dynamic" => Ok(Type::Dynamic),
                other => Err(format!("unknown primitive cty type {other:?}")),
            };
        }
        if let Some(items) = json.as_array() {
            let items = items.as_slice();
            let kind = items
                .first()
                .and_then(Json::as_string)
                .map(|s| s.as_str())
                .ok_or_else(|| "cty type array must start with a kind string".to_string())?;
            return match kind {
                "list" => Ok(Type::list(Self::nth_type(items, 1)?)),
                "set" => Ok(Type::set(Self::nth_type(items, 1)?)),
                "map" => Ok(Type::map(Self::nth_type(items, 1)?)),
                "tuple" => {
                    let elems = items
                        .get(1)
                        .and_then(Json::as_array)
                        .ok_or_else(|| "tuple type needs an element-type array".to_string())?;
                    let tys = elems
                        .as_slice()
                        .iter()
                        .map(Type::from_cty_json)
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(Type::Tuple(tys))
                }
                "object" => {
                    let fields = items
                        .get(1)
                        .and_then(Json::as_object)
                        .ok_or_else(|| "object type needs an attribute map".to_string())?;
                    let optionals: Vec<&str> = items
                        .get(2)
                        .and_then(Json::as_array)
                        .map(|a| {
                            a.as_slice()
                                .iter()
                                .filter_map(|j| j.as_string().map(|s| s.as_str()))
                                .collect()
                        })
                        .unwrap_or_default();
                    let mut attrs = Vec::with_capacity(fields.len());
                    for (name, ty) in fields.iter() {
                        let name = name.as_str();
                        attrs.push(ObjectAttr {
                            name: name.to_string(),
                            ty: Type::from_cty_json(ty)?,
                            optional: optionals.contains(&name),
                        });
                    }
                    Ok(Type::Object(attrs))
                }
                other => Err(format!("unknown cty collection kind {other:?}")),
            };
        }
        Err(format!("invalid cty type constraint: {json:?}"))
    }

    /// Parse a `cty` JSON type constraint from its byte encoding.
    pub fn from_cty_json_bytes(bytes: &[u8]) -> Result<Type, String> {
        let json: Json =
            facet_json::from_slice(bytes).map_err(|e| format!("invalid cty type JSON: {e}"))?;
        Type::from_cty_json(&json)
    }

    /// Helper: parse the element type at `index` of a collection type array.
    fn nth_type(items: &[Json], index: usize) -> Result<Type, String> {
        let elem = items
            .get(index)
            .ok_or_else(|| "collection type missing element type".to_string())?;
        Type::from_cty_json(elem)
    }
}

/// Build a `cty` JSON array value from an iterator of element values.
fn json_array(items: impl IntoIterator<Item = Json>) -> Json {
    Json::from(items.into_iter().collect::<VArray>())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cty type constraint of `ty` as a JSON string.
    fn cty(ty: &Type) -> String {
        String::from_utf8(ty.to_cty_json_bytes()).expect("utf8")
    }

    #[test]
    fn primitive_encoding() {
        assert_eq!(cty(&Type::String), r#""string""#);
        assert_eq!(cty(&Type::Number), r#""number""#);
        assert_eq!(cty(&Type::Bool), r#""bool""#);
        assert_eq!(cty(&Type::Dynamic), r#""dynamic""#);
    }

    #[test]
    fn collection_encoding() {
        assert_eq!(cty(&Type::list(Type::String)), r#"["list","string"]"#);
        assert_eq!(cty(&Type::map(Type::Number)), r#"["map","number"]"#);
        assert_eq!(
            cty(&Type::set(Type::list(Type::Bool))),
            r#"["set",["list","bool"]]"#
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
        assert_eq!(cty(&ty), r#"["object",{"a":"string","b":"number"},["b"]]"#);
    }

    #[test]
    fn object_without_optionals_omits_trailing_list() {
        let ty = Type::Object(vec![ObjectAttr {
            name: "a".into(),
            ty: Type::String,
            optional: false,
        }]);
        assert_eq!(cty(&ty), r#"["object",{"a":"string"}]"#);
    }

    #[test]
    fn cty_json_round_trips() {
        let types = [
            Type::String,
            Type::Number,
            Type::Bool,
            Type::Dynamic,
            Type::list(Type::String),
            Type::set(Type::Bool),
            Type::map(Type::Number),
            Type::Tuple(vec![Type::String, Type::Number]),
            Type::Object(vec![
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
            ]),
        ];
        for ty in types {
            let json = ty.to_cty_json();
            let back = Type::from_cty_json(&json).expect("parses back");
            assert_eq!(back, ty, "round trip for {json:?}");
        }
    }
}
