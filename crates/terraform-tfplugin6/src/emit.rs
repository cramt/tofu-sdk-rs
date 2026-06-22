//! Emitter: provider IR -> Terraform `tfplugin6` schema.
//!
//! This is the "Terraform backend" referenced by the architecture: it is the
//! only place that knows how the backend-agnostic [`terraform_ir`] maps onto the
//! Terraform protocol. Notably, `force_new` is **not** emitted here — it is a
//! planning behavior (`RequiresReplace`), not a schema property, and is consumed
//! by the planning engine in a later phase.

use std::collections::HashMap;

use terraform_ir::{
    AttributeSchema, Block, FunctionSignature, IdentityAttribute, IdentitySchema, NestedBlock,
    NestingMode, Parameter, ProviderSchema,
};

use crate::tfplugin6::{
    self, function, get_metadata, get_provider_schema, get_resource_identity_schemas,
    resource_identity_schema, schema, Function, ResourceIdentitySchema, ServerCapabilities,
    StringKind,
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
/// Resource schemas carry their declared state-schema `version` (Terraform uses
/// it to decide when to call `UpgradeResourceState`); the provider config block
/// and data sources are stateless and emit at version 0.
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
            .map(|r| (r.name.clone(), emit_schema(&r.block, r.version)))
            .collect(),
        data_source_schemas: schema
            .data_sources
            .iter()
            .map(|d| (d.name.clone(), emit_schema(&d.block, 0)))
            .collect(),
        functions: emit_functions(schema),
        ephemeral_resource_schemas: schema
            .ephemeral_resources
            .iter()
            .map(|e| (e.name.clone(), emit_schema(&e.block, 0)))
            .collect(),
        list_resource_schemas: schema
            .list_resources
            .iter()
            .map(|l| (l.name.clone(), emit_schema(&l.config, 0)))
            .collect(),
        state_store_schemas: schema
            .state_stores
            .iter()
            .map(|s| (s.name.clone(), emit_schema(&s.block, 0)))
            .collect(),
        action_schemas: HashMap::new(),
        diagnostics: Vec::new(),
        provider_meta: None,
        server_capabilities: Some(server_capabilities()),
    }
}

/// Build the `GetResourceIdentitySchemas` response: the identity schema of every
/// resource that declares one (resources without an identity are omitted).
pub fn emit_identity_schemas(schema: &ProviderSchema) -> get_resource_identity_schemas::Response {
    get_resource_identity_schemas::Response {
        identity_schemas: schema
            .resources
            .iter()
            .filter_map(|r| {
                r.identity
                    .as_ref()
                    .map(|identity| (r.name.clone(), emit_identity_schema(identity)))
            })
            .collect(),
        diagnostics: Vec::new(),
    }
}

/// Lower an IR [`IdentitySchema`] into a protocol [`ResourceIdentitySchema`].
fn emit_identity_schema(identity: &IdentitySchema) -> ResourceIdentitySchema {
    ResourceIdentitySchema {
        version: identity.version,
        identity_attributes: identity
            .attributes
            .iter()
            .map(emit_identity_attribute)
            .collect(),
    }
}

/// Lower an IR [`IdentityAttribute`] into its protocol form.
fn emit_identity_attribute(
    attr: &IdentityAttribute,
) -> resource_identity_schema::IdentityAttribute {
    resource_identity_schema::IdentityAttribute {
        name: attr.name.clone(),
        r#type: attr.ty.to_cty_json_bytes(),
        required_for_import: attr.required_for_import,
        optional_for_import: attr.optional_for_import,
        description: attr.description.clone().unwrap_or_default(),
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
        functions: schema
            .functions
            .iter()
            .map(|f| get_metadata::FunctionMetadata {
                name: f.name.clone(),
            })
            .collect(),
        ephemeral_resources: schema
            .ephemeral_resources
            .iter()
            .map(|e| get_metadata::EphemeralMetadata {
                type_name: e.name.clone(),
            })
            .collect(),
        list_resources: schema
            .list_resources
            .iter()
            .map(|l| get_metadata::ListResourceMetadata {
                type_name: l.name.clone(),
            })
            .collect(),
        state_stores: schema
            .state_stores
            .iter()
            .map(|s| get_metadata::StateStoreMetadata {
                type_name: s.name.clone(),
            })
            .collect(),
        actions: Vec::new(),
    }
}

/// Lower the IR's function signatures into the protocol's `name -> Function` map
/// (shared by `GetProviderSchema` and `GetFunctions`).
pub fn emit_functions(schema: &ProviderSchema) -> HashMap<String, Function> {
    schema
        .functions
        .iter()
        .map(|f| (f.name.clone(), emit_function(f)))
        .collect()
}

/// Lower one [`FunctionSignature`] into a protocol [`Function`].
fn emit_function(sig: &FunctionSignature) -> Function {
    Function {
        parameters: sig.parameters.iter().map(emit_parameter).collect(),
        variadic_parameter: sig.variadic.as_ref().map(emit_parameter),
        r#return: Some(function::Return {
            r#type: sig.return_type.to_cty_json_bytes(),
        }),
        summary: sig.summary.clone(),
        description: sig.description.clone(),
        description_kind: StringKind::Plain as i32,
        deprecation_message: String::new(),
    }
}

/// Lower one IR [`Parameter`] into a protocol function parameter.
fn emit_parameter(param: &Parameter) -> function::Parameter {
    function::Parameter {
        name: param.name.clone(),
        r#type: param.ty.to_cty_json_bytes(),
        allow_null_value: param.allow_null,
        allow_unknown_values: param.allow_unknown,
        description: param.description.clone(),
        description_kind: StringKind::Plain as i32,
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
        deprecated: attr.deprecated.is_some(),
        write_only: attr.write_only,
        deprecation_message: attr.deprecated.clone().unwrap_or_default(),
    }
}

/// Lower an IR [`NestedBlock`] into a [`schema::NestedBlock`].
fn emit_nested_block(nested: &NestedBlock) -> schema::NestedBlock {
    schema::NestedBlock {
        type_name: nested.name.clone(),
        block: Some(emit_block(&nested.block)),
        nesting: nesting_mode(nested.nesting) as i32,
        min_items: nested.min_items,
        max_items: nested.max_items,
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
