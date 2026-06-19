//! Provider semantic intermediate representation (IR).
//!
//! This is the **stable internal contract** of the SDK. Rust types are reflected
//! into this IR, and backends (Terraform first, others later) are emitted *from*
//! it. The IR intentionally knows nothing about the Terraform plugin protocol,
//! `cty` JSON, msgpack, or gRPC — it only speaks [`terraform_value::Type`] and a
//! small vocabulary of provider concepts.
//!
//! ```text
//! Rust types  ->  facet reflection  ->  [this IR]  ->  Terraform schema emitter
//!                                                  ->  (future) TS / Ruby / WASM
//! ```

use terraform_value::{ObjectAttr, Type, Value};

/// The complete schema for a provider: its own configuration plus every resource
/// and data source it exposes.
///
/// These IR types are `PartialEq` but not `Eq`: an [`AttributeSchema`] can carry
/// a `default` [`Value`], whose `Number` may be a float (no total equality).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProviderSchema {
    /// Provider-level configuration block (e.g. credentials, region).
    pub provider: Option<Block>,
    /// Managed resources keyed by type name (e.g. `aws_s3_bucket`).
    pub resources: Vec<ResourceSchema>,
    /// Read-only data sources keyed by type name.
    pub data_sources: Vec<DataSourceSchema>,
    /// Ephemeral resources keyed by type name (e.g. `aws_session_token`): values
    /// produced for the duration of a single operation and never persisted.
    pub ephemeral_resources: Vec<EphemeralSchema>,
    /// Provider-defined functions, callable from HCL as `provider::<p>::<name>`.
    pub functions: Vec<FunctionSignature>,
}

/// A provider-defined function's signature: its positional parameters, an
/// optional trailing variadic parameter, and its return type. Functions are
/// pure (no provider configuration or state) and run without `ConfigureProvider`.
#[derive(Debug, Clone, PartialEq)]
pub struct FunctionSignature {
    /// The function name, called in HCL as `provider::<provider>::<name>(…)`.
    pub name: String,
    /// The ordered positional parameters.
    pub parameters: Vec<Parameter>,
    /// An optional final parameter accepting zero or more trailing arguments
    /// (Terraform passes them as a list of the parameter type). `None` for a
    /// fixed-arity function.
    pub variadic: Option<Parameter>,
    /// The function's return type.
    pub return_type: Type,
    /// One-line human-readable summary.
    pub summary: String,
    /// Longer human-readable documentation.
    pub description: String,
}

/// A single function parameter.
#[derive(Debug, Clone, PartialEq)]
pub struct Parameter {
    /// Display name for the parameter.
    pub name: String,
    /// The parameter's type constraint.
    pub ty: Type,
    /// Whether a null argument is accepted (true for an `Option<T>` parameter);
    /// when false, Terraform rejects a null argument before calling.
    pub allow_null: bool,
    /// Whether the function tolerates an unknown argument; when false (the
    /// default), Terraform skips the call and assumes an unknown result.
    pub allow_unknown: bool,
    /// Human-readable documentation for the parameter.
    pub description: String,
}

/// A managed resource type.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceSchema {
    /// Fully-qualified type name, e.g. `aws_s3_bucket`.
    pub name: String,
    /// The current state-schema version. Terraform stores this with each
    /// resource's state and calls `UpgradeResourceState` to migrate older state
    /// forward when this number is higher than the stored one.
    pub version: i64,
    /// The resource's attribute/block structure.
    pub block: Block,
}

/// A data source type.
#[derive(Debug, Clone, PartialEq)]
pub struct DataSourceSchema {
    /// Fully-qualified type name, e.g. `aws_s3_bucket`.
    pub name: String,
    /// The data source's attribute/block structure.
    pub block: Block,
}

/// An ephemeral resource type.
///
/// Structurally identical to a [`DataSourceSchema`] — a name plus a [`Block`]
/// (settable config inputs plus computed result attributes) — but driven by the
/// `Open`/`Renew`/`Close` lifecycle rather than a read. Its result is never
/// written to state, so there is no version and no drift.
#[derive(Debug, Clone, PartialEq)]
pub struct EphemeralSchema {
    /// Fully-qualified type name, e.g. `aws_session_token`.
    pub name: String,
    /// The ephemeral resource's attribute/block structure.
    pub block: Block,
}

/// A configuration block: a flat set of attributes plus any nested blocks.
///
/// Mirrors the Terraform notion of a schema block, but without any protocol
/// encoding concerns.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Block {
    /// Scalar / collection attributes declared directly on this block.
    pub attributes: Vec<AttributeSchema>,
    /// Nested blocks (repeatable or singleton sub-structures).
    pub nested_blocks: Vec<NestedBlock>,
}

impl Block {
    /// The `cty` object type a value of this block takes on the wire.
    ///
    /// Used to drive the value codec for this block's `DynamicValue`s. An
    /// attribute is optional in the object type unless it is required; each
    /// nested block contributes an attribute typed by its nesting mode.
    pub fn cty_type(&self) -> Type {
        let mut attrs: Vec<ObjectAttr> = self
            .attributes
            .iter()
            .map(|a| ObjectAttr {
                name: a.name.clone(),
                ty: a.ty.clone(),
                optional: !a.required,
            })
            .collect();

        for nested in &self.nested_blocks {
            let inner = nested.block.cty_type();
            let ty = match nested.nesting {
                NestingMode::Single => inner,
                NestingMode::List => Type::list(inner),
                NestingMode::Set => Type::set(inner),
                NestingMode::Map => Type::map(inner),
            };
            attrs.push(ObjectAttr {
                name: nested.name.clone(),
                ty,
                optional: true,
            });
        }

        Type::Object(attrs)
    }
}

/// A single attribute within a [`Block`].
#[derive(Debug, Clone, PartialEq)]
pub struct AttributeSchema {
    /// Attribute name as written in configuration.
    pub name: String,
    /// The attribute's `cty` type.
    pub ty: Type,
    /// Human-readable description (typically from doc comments).
    pub description: Option<String>,
    /// The caller must set this attribute.
    pub required: bool,
    /// The caller may set this attribute.
    pub optional: bool,
    /// The provider computes this attribute (may be unknown during plan).
    pub computed: bool,
    /// The value is sensitive and should be redacted in UI/logs.
    pub sensitive: bool,
    /// The value is supplied at apply time but is never persisted to state — a
    /// write-only input (e.g. a secret). The runtime nulls it out of every
    /// returned state and the planned state; the real value reaches a handler
    /// only through the apply-time config. Mutually exclusive with `computed`.
    pub write_only: bool,
    /// Changing this attribute forces resource replacement.
    pub force_new: bool,
    /// A default value applied during planning when the caller leaves an
    /// optional attribute unset (null). Not emitted into the Terraform schema —
    /// Terraform has no schema-level default; the provider applies it in the
    /// planner.
    pub default: Option<Value>,
}

impl AttributeSchema {
    /// Create an attribute with all flags cleared.
    pub fn new(name: impl Into<String>, ty: Type) -> Self {
        AttributeSchema {
            name: name.into(),
            ty,
            description: None,
            required: false,
            optional: false,
            computed: false,
            sensitive: false,
            write_only: false,
            force_new: false,
            default: None,
        }
    }
}

/// How a nested block may be repeated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NestingMode {
    /// Exactly one (or zero) instance.
    Single,
    /// An ordered list of instances.
    List,
    /// An unordered set of instances.
    Set,
    /// A string-keyed map of instances.
    Map,
}

/// A nested block within a [`Block`].
#[derive(Debug, Clone, PartialEq)]
pub struct NestedBlock {
    /// Block type name, e.g. `lifecycle_rule`.
    pub name: String,
    /// How the block repeats.
    pub nesting: NestingMode,
    /// The nested block's own structure.
    pub block: Block,
    /// Minimum number of instances. For a [`NestingMode::Single`] block, `1`
    /// means the block is *required* and `0` means optional. For `List`/`Set`/
    /// `Map` it is `0` (a required-non-empty collection can't be inferred from a
    /// Rust `Vec`/`HashSet`/`HashMap` type).
    pub min_items: i64,
    /// Maximum number of instances, or `0` for unbounded. A
    /// [`NestingMode::Single`] block is capped at `1`.
    pub max_items: i64,
}
