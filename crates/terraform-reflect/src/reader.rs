//! Walk a [`facet::Shape`] and lower it into the provider [`terraform_ir`].

use facet::{Def, Facet, Field, PrimitiveType, Shape, Type as FType, UserType};
use terraform_attrs::Attr as TfAttr;
use terraform_ir::{AttributeSchema, Block, DataSourceSchema, ResourceSchema};
use terraform_value::{ObjectAttr, Type};

/// The facet namespace string for our extension attributes.
const NS: &str = "terraform";

/// Errors that can occur while reflecting a Rust type into the IR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReflectError {
    /// The top-level type (or a nested object) was not a struct.
    NotAStruct {
        /// The offending type's identifier.
        type_name: &'static str,
    },
    /// A field had a Rust type with no Terraform `cty` mapping.
    UnsupportedType {
        /// Field path where the unsupported type appeared.
        field: String,
        /// The unsupported type's identifier.
        type_name: &'static str,
    },
    /// A `map`/`HashMap` used a non-string key, which `cty` cannot represent.
    UnsupportedMapKey {
        /// Field path where the bad map appeared.
        field: String,
        /// The key type's identifier.
        key_type: &'static str,
    },
    /// A `#[facet(terraform::search_key(...))]` did not specify exactly one of
    /// `exclusive` or `shared`.
    InvalidSearchKey {
        /// The offending field's name.
        field: String,
    },
}

impl core::fmt::Display for ReflectError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ReflectError::NotAStruct { type_name } => {
                write!(f, "type `{type_name}` is not a struct")
            }
            ReflectError::UnsupportedType { field, type_name } => {
                write!(f, "field `{field}` has unsupported type `{type_name}`")
            }
            ReflectError::UnsupportedMapKey { field, key_type } => write!(
                f,
                "field `{field}` is a map with non-string key type `{key_type}`"
            ),
            ReflectError::InvalidSearchKey { field } => write!(
                f,
                "field `{field}` must use exactly one of \
                 `search_key(exclusive)` or `search_key(shared)`"
            ),
        }
    }
}

impl std::error::Error for ReflectError {}

/// Reflect a Rust type into a [`Block`].
pub fn reflect_block<T: Facet<'static>>() -> Result<Block, ReflectError> {
    block_from_shape(T::SHAPE)
}

/// Reflect a Rust type into a named [`ResourceSchema`].
pub fn reflect_resource<T: Facet<'static>>(
    name: impl Into<String>,
) -> Result<ResourceSchema, ReflectError> {
    Ok(ResourceSchema {
        name: name.into(),
        block: reflect_block::<T>()?,
    })
}

/// Cardinality of a data source search key.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SearchKind {
    /// A unique key — a lookup yields at most one object (singular data source).
    Exclusive,
    /// A generic key — a lookup may yield many objects (plural data source).
    Shared,
}

/// Read a field's `#[facet(terraform::search_key(...))]` cardinality, if any.
/// Errors if the payload sets neither or both of `exclusive`/`shared`.
fn search_kind(field: &'static Field) -> Result<Option<SearchKind>, ReflectError> {
    let Some(attr) = field.get_attr(Some(NS), "search_key") else {
        return Ok(None);
    };
    let invalid = || ReflectError::InvalidSearchKey {
        field: field.name.to_string(),
    };
    let Some(TfAttr::SearchKey(sk)) = attr.get_as::<TfAttr>() else {
        return Err(invalid());
    };
    match (sk.exclusive, sk.shared) {
        (true, false) => Ok(Some(SearchKind::Exclusive)),
        (false, true) => Ok(Some(SearchKind::Shared)),
        _ => Err(invalid()),
    }
}

/// Reflect a struct's fields into `(attribute, search-key cardinality)` pairs.
fn model_attributes(
    shape: &'static Shape,
) -> Result<Vec<(AttributeSchema, Option<SearchKind>)>, ReflectError> {
    let fields = struct_fields(shape)?;
    let mut out = Vec::with_capacity(fields.len());
    for field in fields {
        out.push((attribute_from_field(field)?, search_kind(field)?));
    }
    Ok(out)
}

/// Project a model attribute into a read-only (computed) output: the disposition
/// a non-key field takes in any data source.
fn as_computed(mut attr: AttributeSchema) -> AttributeSchema {
    attr.required = false;
    attr.optional = false;
    attr.computed = true;
    attr.force_new = false;
    attr
}

/// Project a model attribute into a settable lookup input.
fn as_input(mut attr: AttributeSchema, required: bool) -> AttributeSchema {
    attr.required = required;
    attr.optional = !required;
    attr.computed = false;
    attr.force_new = false;
    attr
}

/// Reflect a Rust type into a **singular** [`DataSourceSchema`]: the
/// `search_key(exclusive)` fields become required lookup inputs and every other
/// field becomes a computed output. The data source resolves to a single object.
pub fn reflect_data_source<T: Facet<'static>>(
    name: impl Into<String>,
) -> Result<DataSourceSchema, ReflectError> {
    let attributes = model_attributes(T::SHAPE)?
        .into_iter()
        .map(|(attr, kind)| match kind {
            Some(SearchKind::Exclusive) => as_input(attr, true),
            _ => as_computed(attr),
        })
        .collect();
    Ok(DataSourceSchema {
        name: name.into(),
        block: Block {
            attributes,
            nested_blocks: Vec::new(),
        },
    })
}

/// A reflected **plural** data source: its schema plus the names of the
/// `search_key(shared)` fields that act as lookup inputs (the runtime needs
/// these to project the wrapper back onto the model).
pub struct PluralDataSource {
    /// The reflected schema (shared-key inputs plus a computed `results` list).
    pub schema: DataSourceSchema,
    /// The `search_key(shared)` field names, in declaration order.
    pub shared_keys: Vec<String>,
}

/// Reflect a Rust type into a **plural** data source: the `search_key(shared)`
/// fields become optional lookup inputs and the result is a computed `results`
/// list whose elements are objects of the full model. The data source resolves
/// to any number of objects.
pub fn reflect_data_source_list<T: Facet<'static>>(
    name: impl Into<String>,
) -> Result<PluralDataSource, ReflectError> {
    let attributes = model_attributes(T::SHAPE)?;

    // Each model field is a computed output inside every `results` element.
    let element = Type::Object(
        attributes
            .iter()
            .map(|(attr, _)| ObjectAttr {
                name: attr.name.clone(),
                ty: attr.ty.clone(),
                optional: true,
            })
            .collect(),
    );

    let mut block_attrs = Vec::new();
    let mut shared_keys = Vec::new();
    for (attr, kind) in &attributes {
        if *kind == Some(SearchKind::Shared) {
            shared_keys.push(attr.name.clone());
            block_attrs.push(as_input(attr.clone(), false));
        }
    }
    block_attrs.push(AttributeSchema {
        description: Some("Every object matching the search keys.".to_string()),
        ..as_computed(AttributeSchema::new("results", Type::list(element)))
    });

    Ok(PluralDataSource {
        schema: DataSourceSchema {
            name: name.into(),
            block: Block {
                attributes: block_attrs,
                nested_blocks: Vec::new(),
            },
        },
        shared_keys,
    })
}

/// Build a [`Block`] from a struct shape.
fn block_from_shape(shape: &'static Shape) -> Result<Block, ReflectError> {
    let fields = struct_fields(shape)?;
    let mut attributes = Vec::with_capacity(fields.len());
    for field in fields {
        attributes.push(attribute_from_field(field)?);
    }
    Ok(Block {
        attributes,
        nested_blocks: Vec::new(),
    })
}

/// Extract the field slice of a struct shape, or error if it is not a struct.
fn struct_fields(shape: &'static Shape) -> Result<&'static [Field], ReflectError> {
    match &shape.ty {
        FType::User(UserType::Struct(s)) => Ok(s.fields),
        _ => Err(ReflectError::NotAStruct {
            type_name: shape.type_identifier,
        }),
    }
}

/// Lower a single struct field into an [`AttributeSchema`].
fn attribute_from_field(field: &'static Field) -> Result<AttributeSchema, ReflectError> {
    let name = field.rename.unwrap_or(field.name).to_string();
    let shape = field.shape();
    let is_option = matches!(shape.def, Def::Option(_));
    let ty = map_type(shape, &name)?;

    // Explicit `#[facet(terraform::...)]` flags take precedence.
    let mut required = field.has_attr(Some(NS), "required");
    let mut optional = field.has_attr(Some(NS), "optional");
    let computed = field.has_attr(Some(NS), "computed");
    let force_new = field.has_attr(Some(NS), "force_new");
    let sensitive = field.is_sensitive() || field.has_attr(Some(NS), "sensitive");

    // If the author specified no disposition, infer one: an `Option<T>` field is
    // optional; anything else is required. (A purely computed attribute is left
    // as computed-only.)
    if !required && !optional && !computed {
        if is_option {
            optional = true;
        } else {
            required = true;
        }
    }

    Ok(AttributeSchema {
        name,
        ty,
        description: description(field),
        required,
        optional,
        computed,
        sensitive,
        force_new,
    })
}

/// Join a field's doc-comment lines into an optional description.
fn description(field: &Field) -> Option<String> {
    if field.doc.is_empty() {
        return None;
    }
    let text = field
        .doc
        .iter()
        .map(|line| line.trim())
        .collect::<Vec<_>>()
        .join("\n");
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Map a Rust type's shape to a `cty` [`Type`].
///
/// `field_path` is used only to produce useful error messages.
fn map_type(shape: &'static Shape, field_path: &str) -> Result<Type, ReflectError> {
    // Collections and option are recognized by their semantic `Def`.
    match &shape.def {
        Def::List(d) => return Ok(Type::list(map_type(d.t, field_path)?)),
        Def::Slice(d) => return Ok(Type::list(map_type(d.t, field_path)?)),
        Def::Array(d) => return Ok(Type::list(map_type(d.t, field_path)?)),
        Def::Set(d) => return Ok(Type::set(map_type(d.t, field_path)?)),
        Def::Map(d) => {
            if !is_string_like(d.k) {
                return Err(ReflectError::UnsupportedMapKey {
                    field: field_path.to_string(),
                    key_type: d.k.type_identifier,
                });
            }
            return Ok(Type::map(map_type(d.v, field_path)?));
        }
        // Nullability is handled at the attribute level; nested options just
        // collapse to their inner type for the purposes of the `cty` type.
        Def::Option(d) => return map_type(d.t, field_path),
        _ => {}
    }

    // Scalars and nested structs are recognized by their structural `Type`.
    match &shape.ty {
        FType::Primitive(PrimitiveType::Boolean) => Ok(Type::Bool),
        FType::Primitive(PrimitiveType::Numeric(_)) => Ok(Type::Number),
        FType::Primitive(PrimitiveType::Textual(_)) => Ok(Type::String),
        FType::User(UserType::Struct(s)) => object_type(s.fields, field_path),
        _ if is_string_like(shape) => Ok(Type::String),
        _ => Err(ReflectError::UnsupportedType {
            field: field_path.to_string(),
            type_name: shape.type_identifier,
        }),
    }
}

/// Build a `cty` object type from a nested struct's fields.
fn object_type(fields: &'static [Field], field_path: &str) -> Result<Type, ReflectError> {
    let mut attrs = Vec::with_capacity(fields.len());
    for field in fields {
        let name = field.rename.unwrap_or(field.name);
        let path = format!("{field_path}.{name}");
        let shape = field.shape();
        let optional = matches!(shape.def, Def::Option(_)) || field.has_attr(Some(NS), "optional");
        attrs.push(ObjectAttr {
            name: name.to_string(),
            ty: map_type(shape, &path)?,
            optional,
        });
    }
    Ok(Type::Object(attrs))
}

/// Whether a shape is `String` or `&str`/`str` (a `cty` string).
fn is_string_like(shape: &'static Shape) -> bool {
    if shape.is_type::<String>() {
        return true;
    }
    matches!(shape.ty, FType::Primitive(PrimitiveType::Textual(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use facet::Facet;
    use std::collections::{HashMap, HashSet};
    use terraform_attrs as terraform;

    #[derive(Facet)]
    #[facet(terraform::resource)]
    #[allow(dead_code)]
    struct Bucket {
        /// The name of the bucket.
        #[facet(terraform::required)]
        #[facet(terraform::force_new)]
        name: String,

        #[facet(terraform::computed)]
        arn: String,

        tags: HashMap<String, String>,
        versions: Vec<String>,
        allowed: HashSet<String>,
        retention_days: Option<i64>,
        encrypted: bool,
    }

    fn attr<'a>(block: &'a Block, name: &str) -> &'a AttributeSchema {
        block
            .attributes
            .iter()
            .find(|a| a.name == name)
            .unwrap_or_else(|| panic!("missing attribute `{name}`"))
    }

    #[test]
    fn maps_scalars_collections_and_flags() {
        let block = reflect_block::<Bucket>().expect("Bucket reflects");

        let name = attr(&block, "name");
        assert_eq!(name.ty, Type::String);
        assert!(name.required);
        assert!(name.force_new);
        assert!(!name.computed);
        assert_eq!(name.description.as_deref(), Some("The name of the bucket."));

        let arn = attr(&block, "arn");
        assert!(arn.computed);
        assert!(!arn.required);

        assert_eq!(attr(&block, "tags").ty, Type::map(Type::String));
        assert_eq!(attr(&block, "versions").ty, Type::list(Type::String));
        assert_eq!(attr(&block, "allowed").ty, Type::set(Type::String));
        assert_eq!(attr(&block, "encrypted").ty, Type::Bool);

        // Option<i64> -> optional number.
        let retention = attr(&block, "retention_days");
        assert_eq!(retention.ty, Type::Number);
        assert!(retention.optional);
        assert!(!retention.required);
    }

    #[derive(Facet)]
    #[allow(dead_code)]
    struct Inner {
        a: String,
        b: Option<i64>,
    }

    #[derive(Facet)]
    #[allow(dead_code)]
    struct Outer {
        inner: Inner,
    }

    #[test]
    fn nested_struct_becomes_object_type() {
        let block = reflect_block::<Outer>().expect("Outer reflects");
        let inner = attr(&block, "inner");
        match &inner.ty {
            Type::Object(attrs) => {
                assert_eq!(attrs.len(), 2);
                assert_eq!(attrs[0].name, "a");
                assert_eq!(attrs[0].ty, Type::String);
                assert!(!attrs[0].optional);
                assert_eq!(attrs[1].name, "b");
                assert_eq!(attrs[1].ty, Type::Number);
                assert!(attrs[1].optional, "Option field should be optional");
            }
            other => panic!("expected object type, got {other:?}"),
        }
    }

    #[derive(Facet)]
    #[allow(dead_code)]
    struct BadMap {
        m: HashMap<i64, String>,
    }

    #[test]
    fn non_string_map_key_errors() {
        let err = reflect_block::<BadMap>().unwrap_err();
        assert!(matches!(err, ReflectError::UnsupportedMapKey { .. }));
    }

    // --- data source projections (search keys) ------------------------------

    #[derive(Facet)]
    #[facet(terraform::resource)]
    #[facet(terraform::data_source)]
    #[allow(dead_code)]
    struct Server {
        #[facet(terraform::required)]
        #[facet(terraform::search_key(shared))]
        name: String,

        #[facet(terraform::computed)]
        #[facet(terraform::search_key(exclusive))]
        id: String,

        #[facet(terraform::computed)]
        status: String,
    }

    #[test]
    fn singular_projection_inputs_exclusive_key_computes_rest() {
        let ds = reflect_data_source::<Server>("server").expect("Server reflects");

        let id = attr(&ds.block, "id");
        assert!(
            id.required && !id.computed,
            "exclusive key is a required input"
        );

        let name = attr(&ds.block, "name");
        assert!(
            name.computed && !name.required && !name.optional,
            "a non-exclusive field is a computed output"
        );
        assert!(attr(&ds.block, "status").computed);
    }

    #[test]
    fn plural_projection_inputs_shared_key_wraps_results() {
        let plural = reflect_data_source_list::<Server>("servers").expect("Server reflects");
        assert_eq!(plural.shared_keys, vec!["name".to_string()]);

        let name = attr(&plural.schema.block, "name");
        assert!(
            name.optional && !name.required && !name.computed,
            "the shared key is an optional input"
        );

        // The exclusive-only key is not a plural input; it appears only inside
        // each result object.
        assert!(
            plural
                .schema
                .block
                .attributes
                .iter()
                .all(|a| a.name != "id"),
            "exclusive key should not be a top-level plural input"
        );

        let results = attr(&plural.schema.block, "results");
        assert!(results.computed);
        match &results.ty {
            Type::List(element) => match element.as_ref() {
                Type::Object(attrs) => {
                    let names: Vec<&str> = attrs.iter().map(|a| a.name.as_str()).collect();
                    assert!(names.contains(&"id"));
                    assert!(names.contains(&"name"));
                    assert!(names.contains(&"status"));
                }
                other => panic!("results element should be an object, got {other:?}"),
            },
            other => panic!("results should be a list, got {other:?}"),
        }
    }

    #[derive(Facet)]
    #[facet(terraform::data_source)]
    #[allow(dead_code)]
    struct BadKey {
        #[facet(terraform::search_key(exclusive, shared))]
        key: String,
    }

    #[test]
    fn search_key_with_both_cardinalities_errors() {
        let err = reflect_data_source::<BadKey>("bad").unwrap_err();
        assert!(matches!(err, ReflectError::InvalidSearchKey { .. }));
    }
}
