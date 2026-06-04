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

use terraform_value::Type;

/// The complete schema for a provider: its own configuration plus every resource
/// and data source it exposes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProviderSchema {
    /// Provider-level configuration block (e.g. credentials, region).
    pub provider: Option<Block>,
    /// Managed resources keyed by type name (e.g. `aws_s3_bucket`).
    pub resources: Vec<ResourceSchema>,
    /// Read-only data sources keyed by type name.
    pub data_sources: Vec<DataSourceSchema>,
}

/// A managed resource type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceSchema {
    /// Fully-qualified type name, e.g. `aws_s3_bucket`.
    pub name: String,
    /// The resource's attribute/block structure.
    pub block: Block,
}

/// A data source type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataSourceSchema {
    /// Fully-qualified type name, e.g. `aws_s3_bucket`.
    pub name: String,
    /// The data source's attribute/block structure.
    pub block: Block,
}

/// A configuration block: a flat set of attributes plus any nested blocks.
///
/// Mirrors the Terraform notion of a schema block, but without any protocol
/// encoding concerns.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Block {
    /// Scalar / collection attributes declared directly on this block.
    pub attributes: Vec<AttributeSchema>,
    /// Nested blocks (repeatable or singleton sub-structures).
    pub nested_blocks: Vec<NestedBlock>,
}

/// A single attribute within a [`Block`].
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Changing this attribute forces resource replacement.
    pub force_new: bool,
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
            force_new: false,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NestedBlock {
    /// Block type name, e.g. `lifecycle_rule`.
    pub name: String,
    /// How the block repeats.
    pub nesting: NestingMode,
    /// The nested block's own structure.
    pub block: Block,
}
