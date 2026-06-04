//! Emitter: provider IR -> Terraform `tfplugin6` schema.
//!
//! This is the "Terraform backend" referenced by the architecture: it is the
//! only place that knows how the backend-agnostic [`terraform_ir`] maps onto the
//! Terraform protocol. Notably, `force_new` is **not** emitted here — it is a
//! planning behavior (`RequiresReplace`), not a schema property, and is consumed
//! by the planning engine in a later phase.

use terraform_ir::{AttributeSchema, Block, NestedBlock, NestingMode};

use crate::tfplugin6::{self, schema, StringKind};

/// Lower an IR [`Block`] into a complete [`tfplugin6::Schema`] at `version`.
pub fn emit_schema(block: &Block, version: i64) -> tfplugin6::Schema {
    tfplugin6::Schema {
        version,
        block: Some(emit_block(block)),
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
    let type_json = serde_json::to_vec(&attr.ty.to_cty_json())
        .expect("cty type constraint always serializes to JSON");

    schema::Attribute {
        name: attr.name.clone(),
        r#type: type_json,
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
