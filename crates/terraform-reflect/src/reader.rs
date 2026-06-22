//! Walk a [`facet::Shape`] and lower it into the provider [`terraform_ir`].

use facet::{Def, Facet, Field, PrimitiveType, Shape, Type as FType, UserType};
use terraform_attrs::Attr as TfAttr;
use terraform_ir::{
    AttributeSchema, Block, DataSourceSchema, EphemeralSchema, FunctionSignature,
    IdentityAttribute, IdentitySchema, ListResourceSchema, NestedBlock, NestingMode, Parameter,
    ResourceSchema, StateStoreSchema,
};
use terraform_value::{Number, ObjectAttr, Type, Value};

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
    /// A field is marked both `write_only` and `computed`, which Terraform
    /// rejects — a write-only value is an input the provider never computes.
    WriteOnlyComputed {
        /// The offending field's name.
        field: String,
    },
    /// A list resource's model declared no `#[facet(terraform::identity)]` fields.
    /// A list resource produces resource identities, so its model must have one.
    ListResourceWithoutIdentity {
        /// The list resource's type name.
        name: String,
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
            ReflectError::WriteOnlyComputed { field } => write!(
                f,
                "field `{field}` cannot be both `write_only` and `computed`"
            ),
            ReflectError::ListResourceWithoutIdentity { name } => write!(
                f,
                "list resource `{name}` has no `#[facet(terraform::identity)]` \
                 field; a list resource must produce resource identities"
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
        version: 0,
        block: reflect_block::<T>()?,
        identity: identity_from_shape(T::SHAPE)?,
    })
}

/// Build a resource's [`IdentitySchema`] from its `#[facet(terraform::identity)]`
/// fields. Returns `None` when the model marks no identity fields. Each marked
/// field becomes a `required_for_import` identity attribute carrying the field's
/// own `cty` type.
fn identity_from_shape(shape: &'static Shape) -> Result<Option<IdentitySchema>, ReflectError> {
    let mut attributes = Vec::new();
    for field in struct_fields(shape)? {
        if !field.has_attr(Some(NS), "identity") {
            continue;
        }
        let name = field.rename.unwrap_or(field.name).to_string();
        let ty = map_type(field.shape(), &name)?;
        attributes.push(IdentityAttribute {
            name,
            ty,
            description: description(field),
            required_for_import: true,
            optional_for_import: false,
        });
    }
    Ok((!attributes.is_empty()).then_some(IdentitySchema {
        version: 0,
        attributes,
    }))
}

/// The Terraform type name for a resource model: the explicit name from
/// `#[facet(terraform::resource("name"))]` if present, otherwise the struct
/// identifier converted to `snake_case` (e.g. `AwsS3Bucket` → `aws_s3_bucket`).
pub fn resource_name<T: Facet<'static>>() -> String {
    container_name(T::SHAPE, "resource")
}

/// The Terraform type name for a (singular) data source model — the explicit
/// name from `#[facet(terraform::data_source("name"))]`, else `snake_case(Ident)`.
pub fn data_source_name<T: Facet<'static>>() -> String {
    container_name(T::SHAPE, "data_source")
}

/// The Terraform type name for a **plural** data source model: the singular
/// [`data_source_name`] with an `s` appended (`aws_s3_bucket` → `aws_s3_buckets`),
/// so the same model can back both a singular and a plural data source.
pub fn data_source_list_name<T: Facet<'static>>() -> String {
    format!("{}s", data_source_name::<T>())
}

/// The Terraform type name for an ephemeral resource model — the explicit name
/// from `#[facet(terraform::ephemeral("name"))]`, else `snake_case(Ident)`.
pub fn ephemeral_name<T: Facet<'static>>() -> String {
    container_name(T::SHAPE, "ephemeral")
}

/// Resolve a container's Terraform name: the optional string payload of the
/// `terraform::<key>` attribute, else the snake-cased struct identifier.
fn container_name(shape: &'static Shape, key: &str) -> String {
    explicit_name(shape, key).unwrap_or_else(|| to_snake_case(shape.type_identifier))
}

/// The explicit name from a `#[facet(terraform::<key>("name"))]` container
/// attribute, if one was given. A bare `#[facet(terraform::<key>)]` (or no such
/// attribute) yields `None`.
fn explicit_name(shape: &'static Shape, key: &str) -> Option<String> {
    let attr = shape
        .attributes
        .iter()
        .find(|a| a.ns == Some(NS) && a.key == key)?;
    // The attribute decodes to the grammar enum; both container markers carry an
    // `Option<&'static str>` name (`None` for the bare `resource` form).
    let name = match attr.get_as::<TfAttr>()? {
        TfAttr::Resource(name) | TfAttr::DataSource(name) | TfAttr::Ephemeral(name) => *name,
        _ => return None,
    };
    name.map(str::to_string)
}

/// Convert a PascalCase/CamelCase identifier to `snake_case`. An underscore is
/// inserted before an uppercase letter that follows a lowercase letter or digit,
/// or that begins a new word after an acronym (an uppercase run followed by a
/// lowercase letter). Digits stay attached to the preceding token.
fn to_snake_case(ident: &str) -> String {
    let chars: Vec<char> = ident.chars().collect();
    let mut out = String::with_capacity(ident.len() + 4);
    for (i, &c) in chars.iter().enumerate() {
        if c.is_uppercase() && i > 0 {
            let prev = chars[i - 1];
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            // boundary after a lowercase/digit, or before the last letter of an
            // acronym that starts the next word (e.g. the `B` in `S3Bucket`).
            if prev.is_lowercase() || prev.is_numeric() || (prev.is_uppercase() && next_lower) {
                out.push('_');
            }
        }
        out.extend(c.to_lowercase());
    }
    out
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
    // Write-only is a managed-resource-only disposition; a data-source output is
    // a plain computed value.
    attr.write_only = false;
    attr
}

/// Project a model attribute into a settable lookup input.
fn as_input(mut attr: AttributeSchema, required: bool) -> AttributeSchema {
    attr.required = required;
    attr.optional = !required;
    attr.computed = false;
    attr.force_new = false;
    attr.write_only = false;
    attr
}

/// Project a nested block into a read-only output for a data source: every
/// attribute (recursively, through sub-blocks) becomes computed, and the block
/// is no longer a required input (`min_items` drops to 0).
fn as_computed_block(mut nested: NestedBlock) -> NestedBlock {
    nested.min_items = 0;
    nested.block = computed_block(nested.block);
    nested
}

/// Recursively mark every attribute in `block` (and its sub-blocks) computed.
fn computed_block(block: Block) -> Block {
    Block {
        attributes: block.attributes.into_iter().map(as_computed).collect(),
        nested_blocks: block
            .nested_blocks
            .into_iter()
            .map(as_computed_block)
            .collect(),
    }
}

/// Reflect a Rust type into a **singular** [`DataSourceSchema`]: the
/// `search_key(exclusive)` fields become required lookup inputs and every other
/// field becomes a computed output. The data source resolves to a single object.
pub fn reflect_data_source<T: Facet<'static>>(
    name: impl Into<String>,
) -> Result<DataSourceSchema, ReflectError> {
    let mut attributes = Vec::new();
    let mut nested_blocks = Vec::new();
    for field in struct_fields(T::SHAPE)? {
        if field.has_attr(Some(NS), "block") {
            // A `block` field stays a nested block (read-only), consistent with
            // the resource — rather than collapsing into an object attribute.
            nested_blocks.push(as_computed_block(nested_block_from_field(field)?));
        } else {
            let attr = attribute_from_field(field)?;
            attributes.push(match search_kind(field)? {
                Some(SearchKind::Exclusive) => as_input(attr, true),
                _ => as_computed(attr),
            });
        }
    }
    Ok(DataSourceSchema {
        name: name.into(),
        block: Block {
            attributes,
            nested_blocks,
        },
    })
}

/// Reflect a Rust type into an [`EphemeralSchema`]. Unlike a data source, an
/// ephemeral resource has no search-key projection: its block is the model's
/// attributes as declared — plain fields are settable config inputs, and
/// `#[facet(terraform::computed)]` fields are the result the `open` handler
/// fills. (`force_new` is meaningless here and simply never emitted, as for any
/// block.)
pub fn reflect_ephemeral<T: Facet<'static>>(
    name: impl Into<String>,
) -> Result<EphemeralSchema, ReflectError> {
    Ok(EphemeralSchema {
        name: name.into(),
        block: reflect_block::<T>()?,
    })
}

/// Reflect a Rust type into a [`StateStoreSchema`]. The state store's config type
/// has no search-key or computed-result projection: its block is the model's
/// fields as declared (all settable backend configuration — bucket, region,
/// credentials, …). The `name` is supplied at registration (like a function),
/// since a state store's type name is the backend name, not tied to a model
/// identity.
pub fn reflect_state_store<T: Facet<'static>>(
    name: impl Into<String>,
) -> Result<StateStoreSchema, ReflectError> {
    Ok(StateStoreSchema {
        name: name.into(),
        block: reflect_block::<T>()?,
    })
}

/// Reflect a list resource from the managed resource's `Model` and the list
/// query/filter `Config`. The published schema is `Config`'s block; the identity
/// and full-object type are taken from `Model` (the same model as the managed
/// resource of this `name`), so a listed instance projects into the resource's
/// identity and object by construction. `Model` must declare at least one
/// `#[facet(terraform::identity)]` field — a list resource produces identities.
pub fn reflect_list_resource<Model, Config>(
    name: impl Into<String>,
) -> Result<ListResourceSchema, ReflectError>
where
    Model: Facet<'static>,
    Config: Facet<'static>,
{
    let name = name.into();
    let identity = identity_from_shape(Model::SHAPE)?
        .ok_or_else(|| ReflectError::ListResourceWithoutIdentity { name: name.clone() })?;
    Ok(ListResourceSchema {
        name,
        config: reflect_block::<Config>()?,
        identity,
        object_type: reflect_block::<Model>()?.cty_type(),
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

/// Reflect a provider-defined function's signature from a parameter struct `P`
/// and a return type `O`. Each field of `P` (in declaration order) becomes a
/// positional [`Parameter`] — its name from the field, its type from the field
/// type, and `allow_null` from whether it is an `Option`/`TfValue` — and `O`
/// maps to the return type. Variadic parameters are not yet inferred.
pub fn reflect_function<P: Facet<'static>, O: Facet<'static>>(
    name: impl Into<String>,
) -> Result<FunctionSignature, ReflectError> {
    let parameters = struct_fields(P::SHAPE)?
        .iter()
        .map(parameter_from_field)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(FunctionSignature {
        name: name.into(),
        parameters,
        variadic: None,
        return_type: map_type(O::SHAPE, "return")?,
        summary: String::new(),
        description: String::new(),
    })
}

/// Reflect a **variadic** function: the leading positional parameters from `P`
/// (as in [`reflect_function`]) plus a final variadic parameter of element type
/// `V` (the function accepts zero or more trailing `V` arguments), returning `O`.
pub fn reflect_variadic_function<P, V, O>(
    name: impl Into<String>,
) -> Result<FunctionSignature, ReflectError>
where
    P: Facet<'static>,
    V: Facet<'static>,
    O: Facet<'static>,
{
    let parameters = struct_fields(P::SHAPE)?
        .iter()
        .map(parameter_from_field)
        .collect::<Result<Vec<_>, _>>()?;
    let variadic = Parameter {
        name: "varargs".to_string(),
        ty: map_type(V::SHAPE, "varargs")?,
        allow_null: matches!(V::SHAPE.def, Def::Option(_)) || tfvalue_inner(V::SHAPE).is_some(),
        allow_unknown: false,
        description: String::new(),
    };
    Ok(FunctionSignature {
        name: name.into(),
        parameters,
        variadic: Some(variadic),
        return_type: map_type(O::SHAPE, "return")?,
        summary: String::new(),
        description: String::new(),
    })
}

/// Lower a parameter-struct field into a function [`Parameter`].
fn parameter_from_field(field: &'static Field) -> Result<Parameter, ReflectError> {
    let name = field.rename.unwrap_or(field.name).to_string();
    let shape = field.shape();
    let allow_null = matches!(shape.def, Def::Option(_)) || tfvalue_inner(shape).is_some();
    let ty = map_type(shape, &name)?;
    Ok(Parameter {
        name,
        ty,
        allow_null,
        // Terraform defaults to skipping the call on an unknown argument; a
        // function opting into unknown handling is a future refinement.
        allow_unknown: false,
        description: description(field).unwrap_or_default(),
    })
}

/// Reflect a Rust type into a **plural** data source: the `search_key(shared)`
/// fields become optional lookup inputs and the result is a computed `results`
/// list whose elements are objects of the full model. The data source resolves
/// to any number of objects.
///
/// Unlike the singular projection, a `block` field here renders as an *object
/// attribute* inside the `results` element rather than a nested block — a
/// repeated HCL block can't be an element of a computed `list(object(...))`, so
/// the structure is carried as typed data instead.
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

/// Build a [`Block`] from a struct shape. A field marked
/// `#[facet(terraform::block)]` becomes a [`NestedBlock`] (recursively); every
/// other field becomes an [`AttributeSchema`].
fn block_from_shape(shape: &'static Shape) -> Result<Block, ReflectError> {
    let fields = struct_fields(shape)?;
    let mut attributes = Vec::new();
    let mut nested_blocks = Vec::new();
    for field in fields {
        if field.has_attr(Some(NS), "block") {
            nested_blocks.push(nested_block_from_field(field)?);
        } else {
            attributes.push(attribute_from_field(field)?);
        }
    }
    Ok(Block {
        attributes,
        nested_blocks,
    })
}

/// Lower a `#[facet(terraform::block)]` field into a [`NestedBlock`]. The field's
/// type fixes the nesting mode and the element struct (peeling an outer
/// `Option`): a struct is [`NestingMode::Single`], a list/slice/array is
/// [`NestingMode::List`], a set is [`NestingMode::Set`], and a string-keyed map
/// is [`NestingMode::Map`]. The element struct is reflected recursively, so a
/// block may itself contain attributes and further nested blocks.
///
/// Required-ness is read from the type for *single* blocks: a bare struct is a
/// **required** single block (`min_items = 1`), while an `Option<struct>` is
/// optional (`min_items = 0`). Collection blocks (`Vec`/`HashSet`/`HashMap`) are
/// always `min_items = 0`, since "non-empty" can't be inferred from the type.
fn nested_block_from_field(field: &'static Field) -> Result<NestedBlock, ReflectError> {
    let name = field.rename.unwrap_or(field.name).to_string();

    // An `Option<…>` block is just an optional block; peel it before classifying,
    // but remember it so a single block is marked optional rather than required.
    let optional = matches!(field.shape().def, Def::Option(_));
    let shape = match &field.shape().def {
        Def::Option(d) => d.t,
        _ => field.shape(),
    };

    let (nesting, element) = match &shape.def {
        Def::List(d) => (NestingMode::List, d.t),
        Def::Slice(d) => (NestingMode::List, d.t),
        Def::Array(d) => (NestingMode::List, d.t),
        Def::Set(d) => (NestingMode::Set, d.t),
        Def::Map(d) => {
            if !is_string_like(d.k) {
                return Err(ReflectError::UnsupportedMapKey {
                    field: name,
                    key_type: d.k.type_identifier,
                });
            }
            (NestingMode::Map, d.v)
        }
        // A bare struct (single block).
        _ => (NestingMode::Single, shape),
    };

    // A single block is capped at one instance; required unless `Option`-wrapped.
    // Collections are unbounded (`max_items = 0`) and never required.
    let (min_items, max_items) = match nesting {
        NestingMode::Single if optional => (0, 1),
        NestingMode::Single => (1, 1),
        _ => (0, 0),
    };

    Ok(NestedBlock {
        name,
        nesting,
        block: block_from_shape(element)?,
        min_items,
        max_items,
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

/// The inner type `T` of a `TfValue<T>` shape, or `None` for any other type.
/// `TfValue<T>` is the known/unknown/null wrapper from `terraform-value`; for
/// schema purposes it behaves exactly like its inner `T`, but (like `Option`)
/// makes the attribute nullable.
fn tfvalue_inner(shape: &'static Shape) -> Option<&'static Shape> {
    if shape.type_identifier != "TfValue" {
        return None;
    }
    let FType::User(UserType::Enum(en)) = &shape.ty else {
        return None;
    };
    let known = en.variants.iter().find(|v| v.name == "Known")?;
    Some(known.data.fields.first()?.shape())
}

/// Lower a single struct field into an [`AttributeSchema`].
fn attribute_from_field(field: &'static Field) -> Result<AttributeSchema, ReflectError> {
    let name = field.rename.unwrap_or(field.name).to_string();
    let shape = field.shape();
    // `Option<T>` and `TfValue<T>` are both nullable wrappers for inference.
    let is_option = matches!(shape.def, Def::Option(_)) || tfvalue_inner(shape).is_some();
    let ty = map_type(shape, &name)?;

    // Explicit `#[facet(terraform::...)]` flags. There is no `required` flag:
    // required is always *derived*, never declared.
    let mut optional = field.has_attr(Some(NS), "optional");
    let computed = field.has_attr(Some(NS), "computed");
    let force_new = field.has_attr(Some(NS), "force_new");
    let sensitive = field.is_sensitive() || field.has_attr(Some(NS), "sensitive");
    let write_only = field.has_attr(Some(NS), "write_only");

    // A write-only value is an apply-time input the provider never computes;
    // Terraform rejects the combination outright.
    if write_only && computed {
        return Err(ReflectError::WriteOnlyComputed { field: name });
    }

    // Derive `required`: a field that is neither optional nor computed is
    // required — unless it is a nullable wrapper (`Option<T>`/`TfValue<T>`),
    // which is inferred optional instead.
    let mut required = false;
    if !optional && !computed {
        if is_option {
            optional = true;
        } else {
            required = true;
        }
    }

    let default = field_default(field, &ty);
    let deprecated = field_deprecated(field);

    Ok(AttributeSchema {
        name,
        ty,
        description: description(field),
        required,
        optional,
        computed,
        sensitive,
        write_only,
        force_new,
        deprecated,
        default,
    })
}

/// Read a field's `#[facet(terraform::deprecated)]` / `deprecated("msg")` marker.
/// Returns `Some(message)` (the message may be empty for the bare form) when
/// present, else `None`.
fn field_deprecated(field: &'static Field) -> Option<String> {
    let attr = field.get_attr(Some(NS), "deprecated")?;
    match attr.get_as::<TfAttr>()? {
        TfAttr::Deprecated(message) => Some(message.unwrap_or_default().to_string()),
        _ => None,
    }
}

/// Read a field's `#[facet(terraform::default("…")]` and parse the literal
/// against the attribute's `cty` type: a number for [`Type::Number`], `true`/
/// `false` for [`Type::Bool`], otherwise the string verbatim. An unparseable
/// numeric/bool literal yields no default (the schema still builds).
fn field_default(field: &'static Field, ty: &Type) -> Option<Value> {
    let attr = field.get_attr(Some(NS), "default")?;
    let TfAttr::Default(Some(literal)) = attr.get_as::<TfAttr>()? else {
        return None;
    };
    match ty {
        Type::Number => literal.parse::<Number>().ok().map(Value::Number),
        Type::Bool => literal.parse::<bool>().ok().map(Value::Bool),
        _ => Some(Value::String(literal.to_string())),
    }
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
    // `TfValue<T>` maps to its inner `T`'s cty type (the wrapper only carries the
    // known/unknown/null distinction, which is value-level, not type-level).
    if let Some(inner) = tfvalue_inner(shape) {
        return map_type(inner, field_path);
    }

    // A container-level proxy type (`#[facet(opaque, proxy = P)]`, e.g. a quotient
    // newtype) reflects as its proxy's cty type — the same wire representation the
    // codec encodes it through (`peek_to_value`/`fill` drive the proxy vtable).
    if let Some(proxy) = shape.effective_proxy(None) {
        return map_type(proxy.shape, field_path);
    }

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
        let optional = matches!(shape.def, Def::Option(_))
            || tfvalue_inner(shape).is_some()
            || field.has_attr(Some(NS), "optional");
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
    #[facet(terraform::resource)]
    #[allow(dead_code)]
    struct WithDefaults {
        name: String,
        #[facet(terraform::optional)]
        #[facet(terraform::default("us-east-1"))]
        region: Option<String>,
        #[facet(terraform::optional)]
        #[facet(terraform::default("3"))]
        retries: Option<i64>,
        #[facet(terraform::optional)]
        #[facet(terraform::default("true"))]
        enabled: Option<bool>,
    }

    #[test]
    fn default_literal_parses_against_attribute_type() {
        let block = reflect_block::<WithDefaults>().expect("WithDefaults reflects");
        assert_eq!(
            attr(&block, "region").default,
            Some(Value::String("us-east-1".into()))
        );
        assert_eq!(attr(&block, "retries").default, Some(Value::from(3.0)));
        assert_eq!(attr(&block, "enabled").default, Some(Value::Bool(true)));
        assert_eq!(attr(&block, "name").default, None);
    }

    #[derive(Facet)]
    #[facet(terraform::resource)]
    #[allow(dead_code)]
    struct WithTfValue {
        name: String,
        // A `TfValue<T>` field reflects to T's cty type and is nullable
        // (optional), like an `Option`.
        token: terraform_value::TfValue<String>,
        #[facet(terraform::computed)]
        size: terraform_value::TfValue<i64>,
    }

    #[test]
    fn tfvalue_field_maps_to_inner_type_and_is_optional() {
        let block = reflect_block::<WithTfValue>().expect("WithTfValue reflects");
        let token = attr(&block, "token");
        assert_eq!(token.ty, Type::String, "TfValue<String> -> string");
        assert!(token.optional, "TfValue field is nullable -> optional");
        assert!(!token.required);

        let size = attr(&block, "size");
        assert_eq!(size.ty, Type::Number, "TfValue<i64> -> number");
        assert!(size.computed);
    }

    // A string-backed quotient type: reflects through its `String` proxy (matching
    // how the codec encodes/decodes it). See `terraform-codec` for the value-level
    // round-trip and `terraform-runtime::normalize` for the semantic-equality use.
    #[derive(Facet)]
    #[facet(opaque, proxy = String)]
    #[allow(dead_code)]
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
    #[allow(dead_code)]
    struct WithQuotient {
        id: CiId,
        alias: Option<CiId>,
    }

    #[test]
    fn proxy_field_maps_to_proxy_cty_type() {
        let block = reflect_block::<WithQuotient>().expect("WithQuotient reflects");
        let id = attr(&block, "id");
        assert_eq!(id.ty, Type::String, "opaque+proxy=String -> string");
        assert!(id.required, "a bare quotient field is required");

        // `Option<Quotient>` still maps to the proxy type, and stays optional.
        let alias = attr(&block, "alias");
        assert_eq!(alias.ty, Type::String);
        assert!(alias.optional);
        assert!(!alias.required);
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

    // --- nested blocks ------------------------------------------------------

    #[derive(Facet, Hash, PartialEq, Eq)]
    #[allow(dead_code)]
    struct Rule {
        port: String,
    }

    #[derive(Facet)]
    #[allow(dead_code)]
    struct Meta {
        author: String,
        note: Option<String>,
    }

    #[derive(Facet)]
    #[facet(terraform::resource)]
    #[allow(dead_code)]
    struct Firewall {
        name: String,

        // Single block (optional): `Option<…>` ⇒ min_items 0.
        #[facet(terraform::block)]
        meta: Option<Meta>,

        // Single block (required): a bare struct ⇒ min_items 1.
        #[facet(terraform::block)]
        policy: Meta,

        // List, set, and map blocks of the same element struct.
        #[facet(terraform::block)]
        ingress: Vec<Rule>,
        #[facet(terraform::block)]
        egress: HashSet<Rule>,
        #[facet(terraform::block)]
        named: HashMap<String, Rule>,
    }

    fn nested<'a>(block: &'a Block, name: &str) -> &'a NestedBlock {
        block
            .nested_blocks
            .iter()
            .find(|b| b.name == name)
            .unwrap_or_else(|| panic!("missing nested block `{name}`"))
    }

    #[test]
    fn block_marker_emits_nested_blocks_with_nesting_modes() {
        let block = reflect_block::<Firewall>().expect("Firewall reflects");

        // `name` stays a plain attribute; the five block fields are nested blocks.
        assert_eq!(block.attributes.len(), 1);
        assert_eq!(block.attributes[0].name, "name");
        assert_eq!(block.nested_blocks.len(), 5);

        assert_eq!(nested(&block, "meta").nesting, NestingMode::Single);
        assert_eq!(nested(&block, "policy").nesting, NestingMode::Single);
        assert_eq!(nested(&block, "ingress").nesting, NestingMode::List);
        assert_eq!(nested(&block, "egress").nesting, NestingMode::Set);
        assert_eq!(nested(&block, "named").nesting, NestingMode::Map);
    }

    #[test]
    fn single_block_required_ness_comes_from_the_type() {
        let block = reflect_block::<Firewall>().expect("Firewall reflects");

        // `Option<Meta>` ⇒ optional single block.
        let meta = nested(&block, "meta");
        assert_eq!((meta.min_items, meta.max_items), (0, 1));

        // bare `Meta` ⇒ required single block.
        let policy = nested(&block, "policy");
        assert_eq!((policy.min_items, policy.max_items), (1, 1));

        // collections are unbounded and never required.
        let ingress = nested(&block, "ingress");
        assert_eq!((ingress.min_items, ingress.max_items), (0, 0));
    }

    #[test]
    fn nested_block_reflects_element_attributes() {
        let block = reflect_block::<Firewall>().expect("Firewall reflects");

        // The single block's element struct keeps its attribute dispositions.
        let meta = &nested(&block, "meta").block;
        assert_eq!(attr(meta, "author").ty, Type::String);
        assert!(attr(meta, "author").required);
        assert!(attr(meta, "note").optional, "Option field is optional");

        // The repeatable blocks carry the element struct's attribute.
        assert!(attr(&nested(&block, "ingress").block, "port").required);
    }

    #[derive(Facet)]
    #[allow(dead_code)]
    struct Level2 {
        value: String,
    }

    #[derive(Facet)]
    #[allow(dead_code)]
    struct Level1 {
        label: String,
        #[facet(terraform::block)]
        deep: Vec<Level2>,
    }

    #[derive(Facet)]
    #[facet(terraform::resource)]
    #[allow(dead_code)]
    struct Nested {
        name: String,
        #[facet(terraform::block)]
        level1: Vec<Level1>,
    }

    #[test]
    fn blocks_nest_recursively() {
        let block = reflect_block::<Nested>().expect("Nested reflects");
        let l1 = &nested(&block, "level1").block;
        assert!(attr(l1, "label").required);
        let l2 = &nested(l1, "deep").block;
        assert_eq!(l2.nested_blocks.len(), 0);
        assert!(attr(l2, "value").required);
    }

    #[derive(Facet)]
    #[facet(terraform::resource)]
    #[allow(dead_code)]
    struct BadBlock {
        #[facet(terraform::block)]
        oops: String,
    }

    #[test]
    fn block_on_non_struct_errors() {
        let err = reflect_block::<BadBlock>().unwrap_err();
        assert!(matches!(err, ReflectError::NotAStruct { .. }));
    }

    // --- resource name resolution ------------------------------------------

    #[derive(Facet)]
    #[facet(terraform::resource("aws_s3_bucket"))]
    #[allow(dead_code)]
    struct NamedResource {
        id: String,
    }

    #[derive(Facet)]
    #[facet(terraform::resource)]
    #[allow(dead_code)]
    struct AwsS3Bucket {
        id: String,
    }

    #[test]
    fn resource_name_prefers_explicit_attribute() {
        assert_eq!(resource_name::<NamedResource>(), "aws_s3_bucket");
    }

    #[test]
    fn resource_name_infers_snake_case_from_struct() {
        // No explicit name: fall back to snake_case of the struct identifier,
        // keeping the digit attached (`AwsS3Bucket` -> `aws_s3_bucket`).
        assert_eq!(resource_name::<AwsS3Bucket>(), "aws_s3_bucket");
    }

    #[test]
    fn snake_case_conversion_cases() {
        assert_eq!(to_snake_case("Bucket"), "bucket");
        assert_eq!(to_snake_case("FileModel"), "file_model");
        assert_eq!(to_snake_case("AwsS3Bucket"), "aws_s3_bucket");
        assert_eq!(to_snake_case("HTTPServer"), "http_server");
    }

    #[derive(Facet)]
    #[facet(terraform::data_source("aws_s3_bucket"))]
    #[allow(dead_code)]
    struct NamedDataSource {
        #[facet(terraform::search_key(exclusive))]
        id: String,
    }

    #[test]
    fn data_source_name_singular_and_plural() {
        // The singular name comes from the marker; the plural appends `s`.
        assert_eq!(data_source_name::<NamedDataSource>(), "aws_s3_bucket");
        assert_eq!(data_source_list_name::<NamedDataSource>(), "aws_s3_buckets");
    }

    #[test]
    fn data_source_name_infers_from_struct() {
        // No explicit name: snake_case, and the plural appends `s`.
        assert_eq!(data_source_name::<AwsS3Bucket>(), "aws_s3_bucket");
        assert_eq!(data_source_list_name::<AwsS3Bucket>(), "aws_s3_buckets");
    }

    // --- data source projections (search keys) ------------------------------

    #[derive(Facet)]
    #[facet(terraform::resource)]
    #[facet(terraform::data_source)]
    #[allow(dead_code)]
    struct Server {
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

    #[derive(Facet)]
    #[allow(dead_code)]
    struct HostConfig {
        region: String,
        zone: Option<String>,
    }

    #[derive(Facet)]
    #[facet(terraform::data_source)]
    #[allow(dead_code)]
    struct Host {
        #[facet(terraform::search_key(exclusive))]
        id: String,
        #[facet(terraform::computed)]
        name: String,
        // A nested block on the model: it must stay a nested block (read-only) in
        // the data source, not collapse into an object attribute.
        #[facet(terraform::block)]
        config: HostConfig,
    }

    #[test]
    fn singular_projection_keeps_block_as_computed_nested_block() {
        let ds = reflect_data_source::<Host>("host").expect("Host reflects");

        // `config` is not flattened into an attribute…
        assert!(
            ds.block.attributes.iter().all(|a| a.name != "config"),
            "a block field must not collapse into an object attribute"
        );
        // …it stays a nested block, projected read-only: no longer a required
        // input (min_items drops to 0) and its inner attributes are computed.
        let config = nested(&ds.block, "config");
        assert_eq!(config.nesting, NestingMode::Single);
        assert_eq!(
            config.min_items, 0,
            "an output block is not a required input"
        );
        assert!(attr(&config.block, "region").computed);
        assert!(attr(&config.block, "zone").computed);
    }

    // --- functions ----------------------------------------------------------

    #[derive(Facet)]
    #[allow(dead_code)]
    struct ConcatArgs {
        prefix: String,
        count: i64,
        suffix: Option<String>,
    }

    #[test]
    fn reflect_function_maps_params_in_order_and_return() {
        let sig = reflect_function::<ConcatArgs, String>("concat").expect("reflects");
        assert_eq!(sig.name, "concat");
        assert!(sig.variadic.is_none());
        assert_eq!(sig.return_type, Type::String);

        assert_eq!(sig.parameters.len(), 3);
        assert_eq!(sig.parameters[0].name, "prefix");
        assert_eq!(sig.parameters[0].ty, Type::String);
        assert!(!sig.parameters[0].allow_null);
        assert_eq!(sig.parameters[1].name, "count");
        assert_eq!(sig.parameters[1].ty, Type::Number);
        // `Option<String>` ⇒ a nullable argument.
        assert_eq!(sig.parameters[2].name, "suffix");
        assert!(sig.parameters[2].allow_null);
    }

    #[derive(Facet)]
    #[allow(dead_code)]
    struct JoinArgs {
        separator: String,
    }

    #[test]
    fn reflect_variadic_function_separates_leading_and_variadic() {
        // Leading `separator: String`, variadic element `i64`, returns `String`.
        let sig = reflect_variadic_function::<JoinArgs, i64, String>("join").expect("reflects");
        assert_eq!(sig.parameters.len(), 1, "one fixed leading parameter");
        assert_eq!(sig.parameters[0].name, "separator");
        assert_eq!(sig.parameters[0].ty, Type::String);

        let variadic = sig.variadic.expect("a variadic parameter");
        assert_eq!(
            variadic.ty,
            Type::Number,
            "variadic element type is the number"
        );
        assert_eq!(sig.return_type, Type::String);
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

    // --- ephemeral resources ------------------------------------------------

    #[derive(Facet)]
    #[facet(terraform::ephemeral("aws_session_token"))]
    #[allow(dead_code)]
    struct SessionToken {
        role: String,
        #[facet(terraform::optional)]
        ttl_seconds: Option<i64>,
        #[facet(terraform::computed)]
        #[facet(terraform::sensitive)]
        token: String,
    }

    // --- write-only attributes ---------------------------------------------

    #[derive(Facet)]
    #[facet(terraform::resource)]
    #[allow(dead_code)]
    struct WoModel {
        name: String,
        #[facet(terraform::write_only)]
        password: Option<String>,
    }

    #[test]
    fn write_only_flag_is_reflected_as_optional_input() {
        let block = reflect_block::<WoModel>().expect("reflects");
        let password = attr(&block, "password");
        assert!(password.write_only, "field is marked write-only");
        assert!(
            password.optional && !password.required && !password.computed,
            "a write-only input is optional, never computed"
        );
    }

    #[derive(Facet)]
    #[facet(terraform::resource)]
    #[allow(dead_code)]
    struct WoComputedModel {
        name: String,
        #[facet(terraform::write_only)]
        #[facet(terraform::computed)]
        bad: String,
    }

    #[test]
    fn write_only_with_computed_is_rejected() {
        let err =
            reflect_block::<WoComputedModel>().expect_err("must reject write_only + computed");
        assert!(matches!(err, ReflectError::WriteOnlyComputed { .. }));
    }

    #[derive(Facet)]
    #[facet(terraform::resource)]
    #[allow(dead_code)]
    struct DeprecatedModel {
        name: String,
        #[facet(terraform::deprecated("use `name` instead"))]
        legacy_name: Option<String>,
        #[facet(terraform::deprecated)]
        old_flag: Option<bool>,
    }

    #[derive(Facet)]
    #[facet(terraform::resource)]
    #[allow(dead_code)]
    struct IdentityModel {
        name: String,
        #[facet(terraform::computed)]
        #[facet(terraform::identity)]
        arn: String,
    }

    #[test]
    fn identity_is_projected_from_marked_fields() {
        let resource = reflect_resource::<IdentityModel>("identity_model").expect("reflects");
        let identity = resource.identity.expect("declares an identity");
        assert_eq!(identity.version, 0);
        assert_eq!(identity.attributes.len(), 1, "only the marked field");
        let arn = &identity.attributes[0];
        assert_eq!(arn.name, "arn");
        assert_eq!(
            arn.ty,
            Type::String,
            "identity carries the field's cty type"
        );
        assert!(
            arn.required_for_import,
            "identity attributes are required for import by default"
        );
    }

    #[test]
    fn no_identity_marker_yields_no_identity_schema() {
        let resource = reflect_resource::<Bucket>("aws_s3_bucket").expect("reflects");
        assert!(
            resource.identity.is_none(),
            "a model without identity markers has no identity schema"
        );
    }

    #[test]
    fn deprecated_marker_carries_optional_message() {
        let block = reflect_block::<DeprecatedModel>().expect("reflects");
        assert_eq!(
            attr(&block, "legacy_name").deprecated.as_deref(),
            Some("use `name` instead"),
            "deprecated with a message"
        );
        assert_eq!(
            attr(&block, "old_flag").deprecated.as_deref(),
            Some(""),
            "bare deprecated carries an empty message"
        );
        assert_eq!(
            attr(&block, "name").deprecated,
            None,
            "unmarked attribute is not deprecated"
        );
    }

    #[test]
    fn ephemeral_name_prefers_explicit_then_snake_case() {
        assert_eq!(ephemeral_name::<SessionToken>(), "aws_session_token");
        assert_eq!(ephemeral_name::<AwsS3Bucket>(), "aws_s3_bucket");
    }

    #[test]
    fn reflect_ephemeral_keeps_inputs_and_computed_outputs() {
        let eph = reflect_ephemeral::<SessionToken>("aws_session_token").expect("reflects");
        assert_eq!(eph.name, "aws_session_token");

        let role = attr(&eph.block, "role");
        assert!(
            role.required && !role.computed,
            "plain field is a required input"
        );

        let ttl = attr(&eph.block, "ttl_seconds");
        assert!(
            ttl.optional && !ttl.required,
            "Option field is an optional input"
        );

        let token = attr(&eph.block, "token");
        assert!(
            token.computed && token.sensitive,
            "computed sensitive result"
        );
    }

    #[derive(Facet)]
    #[allow(dead_code)]
    struct StateStoreConfig {
        /// A required input.
        bucket: String,
        /// An optional input.
        region: Option<String>,
    }

    #[test]
    fn reflect_state_store_projects_config_block() {
        let store = reflect_state_store::<StateStoreConfig>("s3").expect("reflects");
        assert_eq!(store.name, "s3");

        let bucket = attr(&store.block, "bucket");
        assert!(
            bucket.required && !bucket.computed,
            "plain field is a required config input"
        );
        let region = attr(&store.block, "region");
        assert!(
            region.optional && !region.required,
            "Option field is an optional config input"
        );
    }

    #[derive(Facet)]
    #[allow(dead_code)]
    struct ListFilter {
        prefix: Option<String>,
    }

    #[test]
    fn reflect_list_resource_uses_config_block_and_model_identity() {
        let list =
            reflect_list_resource::<IdentityModel, ListFilter>("identity_model").expect("reflects");
        assert_eq!(list.name, "identity_model");
        // The published schema is the *config* type, not the model.
        assert_eq!(list.config.attributes.len(), 1);
        assert_eq!(list.config.attributes[0].name, "prefix");
        // Identity is the model's (a list resource produces resource identities).
        assert_eq!(list.identity.attributes.len(), 1);
        assert_eq!(list.identity.attributes[0].name, "arn");
        // The object type is the managed resource's full object (name + arn).
        let Type::Object(attrs) = &list.object_type else {
            panic!("object type should be an object");
        };
        assert!(attrs.iter().any(|a| a.name == "name"));
        assert!(attrs.iter().any(|a| a.name == "arn"));
    }

    #[test]
    fn reflect_list_resource_requires_model_identity() {
        // `SessionToken` declares no identity field — a list resource needs one.
        let err = reflect_list_resource::<SessionToken, ListFilter>("aws_session_token")
            .expect_err("must reject a model without identity");
        assert!(matches!(
            err,
            ReflectError::ListResourceWithoutIdentity { .. }
        ));
    }
}
