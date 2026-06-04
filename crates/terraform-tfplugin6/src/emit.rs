//! Emitter: provider IR -> Terraform `tfplugin6` schema.
//!
//! This is the "Terraform backend" referenced by the architecture: it is the
//! only place that knows how the backend-agnostic [`terraform_ir`] maps onto the
//! Terraform protocol. Notably, `force_new` is **not** emitted here — it is a
//! planning behavior (`RequiresReplace`), not a schema property, and is consumed
//! by the planning engine in a later phase.

use std::collections::HashMap;

use terraform_ir::{AttributeSchema, Block, NestedBlock, NestingMode, ProviderSchema};

use crate::tfplugin6::{
    self, get_metadata, get_provider_schema, schema, ServerCapabilities, StringKind,
};

/// Lower an IR [`Block`] into a complete [`tfplugin6::Schema`] at `version`.
pub fn emit_schema(block: &Block, version: i64) -> tfplugin6::Schema {
    tfplugin6::Schema {
        version,
        block: Some(emit_block(block)),
    }
}

/// Lower a whole [`ProviderSchema`] into a `GetProviderSchema` response.
///
/// All schemas are emitted at state-schema version 0; per-resource versioning
/// (for state upgrades) arrives with the planning/upgrade work in a later phase.
pub fn emit_provider_schema(schema: &ProviderSchema) -> get_provider_schema::Response {
    // Terraform requires the provider schema to always be present, even when the
    // provider takes no configuration — an absent `provider` field is reported as
    // "missing provider schema". Default to an empty block in that case.
    let provider = schema
        .provider
        .as_ref()
        .map(|b| emit_schema(b, 0))
        .unwrap_or_else(|| emit_schema(&Block::default(), 0));

    get_provider_schema::Response {
        provider: Some(provider),
        resource_schemas: schema
            .resources
            .iter()
            .map(|r| (r.name.clone(), emit_schema(&r.block, 0)))
            .collect(),
        data_source_schemas: schema
            .data_sources
            .iter()
            .map(|d| (d.name.clone(), emit_schema(&d.block, 0)))
            .collect(),
        functions: HashMap::new(),
        ephemeral_resource_schemas: HashMap::new(),
        list_resource_schemas: HashMap::new(),
        state_store_schemas: HashMap::new(),
        action_schemas: HashMap::new(),
        diagnostics: Vec::new(),
        provider_meta: None,
        server_capabilities: Some(server_capabilities()),
    }
}

/// Lower a [`ProviderSchema`] into a `GetMetadata` response (type-name listing).
pub fn emit_metadata(schema: &ProviderSchema) -> get_metadata::Response {
    get_metadata::Response {
        server_capabilities: Some(server_capabilities()),
        diagnostics: Vec::new(),
        data_sources: schema
            .data_sources
            .iter()
            .map(|d| get_metadata::DataSourceMetadata {
                type_name: d.name.clone(),
            })
            .collect(),
        resources: schema
            .resources
            .iter()
            .map(|r| get_metadata::ResourceMetadata {
                type_name: r.name.clone(),
            })
            .collect(),
        functions: Vec::new(),
        ephemeral_resources: Vec::new(),
        list_resources: Vec::new(),
        state_stores: Vec::new(),
        actions: Vec::new(),
    }
}

/// The capabilities this SDK currently advertises.
///
/// Everything is `false` for now: the planning, move, and config-generation RPCs
/// are not yet implemented, so we must not claim them.
pub fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        plan_destroy: false,
        get_provider_schema_optional: false,
        move_resource_state: false,
        generate_resource_config: false,
    }
}

/// Lower an IR [`Block`] into a [`schema::Block`].
pub fn emit_block(block: &Block) -> schema::Block {
    schema::Block {
        version: 0,
        attributes: block.attributes.iter().map(emit_attribute).collect(),
        block_types: block.nested_blocks.iter().map(emit_nested_block).collect(),
        description: String::new(),
        description_kind: StringKind::Plain as i32,
        deprecated: false,
        deprecation_message: String::new(),
        computed: false,
    }
}

/// Lower an IR [`AttributeSchema`] into a [`schema::Attribute`].
///
/// The attribute's `cty` type is serialized to the JSON type-constraint encoding
/// Terraform expects in the `type` field.
fn emit_attribute(attr: &AttributeSchema) -> schema::Attribute {
    schema::Attribute {
        name: attr.name.clone(),
        r#type: attr.ty.to_cty_json_bytes(),
        nested_type: None,
        description: attr.description.clone().unwrap_or_default(),
        required: attr.required,
        optional: attr.optional,
        computed: attr.computed,
        sensitive: attr.sensitive,
        description_kind: StringKind::Plain as i32,
        deprecated: false,
        write_only: false,
        deprecation_message: String::new(),
    }
}

/// Lower an IR [`NestedBlock`] into a [`schema::NestedBlock`].
fn emit_nested_block(nested: &NestedBlock) -> schema::NestedBlock {
    schema::NestedBlock {
        type_name: nested.name.clone(),
        block: Some(emit_block(&nested.block)),
        nesting: nesting_mode(nested.nesting) as i32,
        min_items: 0,
        max_items: 0,
    }
}

/// Map the IR nesting mode to the protocol enum.
fn nesting_mode(mode: NestingMode) -> schema::nested_block::NestingMode {
    use schema::nested_block::NestingMode as Pb;
    match mode {
        NestingMode::Single => Pb::Single,
        NestingMode::List => Pb::List,
        NestingMode::Set => Pb::Set,
        NestingMode::Map => Pb::Map,
    }
}
