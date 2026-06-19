//! Write-only attribute handling.
//!
//! A `write_only` attribute is an apply-time input (typically a secret) that is
//! **never persisted to state**. Terraform sends its real value in the *config*
//! of a plan/apply, but the prior/planned/new state always carry it as null. The
//! provider must honour that: every state it returns — and the planned state —
//! has write-only attributes nulled, while a handler still needs the real value
//! at apply time.
//!
//! Two operations cover this:
//!
//! - [`strip`] nulls every write-only attribute in a value (the state we hand
//!   back to Terraform), recursing through nested blocks.
//! - [`merge_from_config`] copies the real write-only values out of the
//!   apply-time config into the planned value, so a `create`/`update` handler
//!   receives them before [`strip`] removes them from the result.
//!
//! [`block_has`] lets the service skip both walks (and the config decode) for the
//! overwhelmingly common case of a resource with no write-only attributes.

use terraform_ir::{Block, NestingMode};
use terraform_value::Value;

/// Does `block` (recursively, through nested blocks) declare any write-only
/// attribute? If not, the runtime can skip all write-only handling.
pub fn block_has(block: &Block) -> bool {
    block.attributes.iter().any(|attr| attr.write_only)
        || block
            .nested_blocks
            .iter()
            .any(|nested| block_has(&nested.block))
}

/// Null every write-only attribute in `value`, recursing into nested blocks.
/// Non-object values pass through untouched.
pub fn strip(value: &mut Value, block: &Block) {
    let Value::Object(fields) = value else {
        return;
    };
    for attr in &block.attributes {
        if attr.write_only {
            if let Some(slot) = fields.get_mut(&attr.name) {
                *slot = Value::Null;
            }
        }
    }
    for nested in &block.nested_blocks {
        if let Some(child) = fields.get_mut(&nested.name) {
            walk_block(child, &nested.block, nested.nesting, &mut |v, b| {
                strip(v, b)
            });
        }
    }
}

/// Copy write-only attribute values from `config` into `state` so an apply
/// handler receives the real (never-persisted) value. Recurses into nested
/// blocks, aligning list elements by index and map entries by key. **Set blocks
/// are skipped** — set elements have no stable identity to align by, and
/// Terraform does not allow write-only attributes inside set-nested blocks.
pub fn merge_from_config(state: &mut Value, config: &Value, block: &Block) {
    let (Value::Object(state_fields), Value::Object(config_fields)) = (state, config) else {
        return;
    };
    for attr in &block.attributes {
        if attr.write_only {
            if let Some(value) = config_fields.get(&attr.name) {
                state_fields.insert(attr.name.clone(), value.clone());
            }
        }
    }
    for nested in &block.nested_blocks {
        let (Some(state_child), Some(config_child)) = (
            state_fields.get_mut(&nested.name),
            config_fields.get(&nested.name),
        ) else {
            continue;
        };
        match nested.nesting {
            NestingMode::Single => merge_from_config(state_child, config_child, &nested.block),
            NestingMode::List => {
                if let (Value::List(state_items), Value::List(config_items)) =
                    (state_child, config_child)
                {
                    for (s, c) in state_items.iter_mut().zip(config_items.iter()) {
                        merge_from_config(s, c, &nested.block);
                    }
                }
            }
            NestingMode::Map => {
                if let (Value::Map(state_entries), Value::Map(config_entries)) =
                    (state_child, config_child)
                {
                    for (key, s) in state_entries.iter_mut() {
                        if let Some(c) = config_entries.get(key) {
                            merge_from_config(s, c, &nested.block);
                        }
                    }
                }
            }
            // Set elements can't be aligned; write-only inside a set block is
            // unsupported by Terraform, so there is nothing to merge.
            NestingMode::Set => {}
        }
    }
}

/// Apply `op` to each block-element value per its nesting mode (a single object,
/// or each element of a list/set/map). Non-matching shapes pass through.
fn walk_block(
    value: &mut Value,
    block: &Block,
    nesting: NestingMode,
    op: &mut dyn FnMut(&mut Value, &Block),
) {
    match nesting {
        NestingMode::Single => op(value, block),
        NestingMode::List | NestingMode::Set => {
            if let Value::List(items) | Value::Set(items) = value {
                for item in items.iter_mut() {
                    op(item, block);
                }
            }
        }
        NestingMode::Map => {
            if let Value::Map(entries) = value {
                for entry in entries.values_mut() {
                    op(entry, block);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use terraform_ir::{AttributeSchema, Block, NestedBlock, NestingMode};
    use terraform_value::{Type, Value};

    use super::*;

    fn obj(pairs: &[(&str, Value)]) -> Value {
        Value::Object(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    fn fields(v: &Value) -> &BTreeMap<String, Value> {
        match v {
            Value::Object(m) => m,
            _ => panic!("expected object"),
        }
    }

    fn block_with_secret() -> Block {
        Block {
            attributes: vec![
                AttributeSchema {
                    required: true,
                    ..AttributeSchema::new("name", Type::String)
                },
                AttributeSchema {
                    optional: true,
                    write_only: true,
                    ..AttributeSchema::new("password", Type::String)
                },
            ],
            nested_blocks: Vec::new(),
        }
    }

    #[test]
    fn block_has_detects_write_only() {
        assert!(block_has(&block_with_secret()));
        assert!(!block_has(&Block {
            attributes: vec![AttributeSchema::new("name", Type::String)],
            nested_blocks: Vec::new(),
        }));
    }

    #[test]
    fn strip_nulls_top_level_write_only() {
        let mut v = obj(&[
            ("name", Value::String("db".into())),
            ("password", Value::String("hunter2".into())),
        ]);
        strip(&mut v, &block_with_secret());
        assert_eq!(fields(&v)["name"], Value::String("db".into()));
        assert!(fields(&v)["password"].is_null());
    }

    #[test]
    fn merge_pulls_write_only_from_config() {
        // Planned state nulls the secret; config carries the real value.
        let mut planned = obj(&[
            ("name", Value::String("db".into())),
            ("password", Value::Null),
        ]);
        let config = obj(&[
            ("name", Value::String("db".into())),
            ("password", Value::String("hunter2".into())),
        ]);
        merge_from_config(&mut planned, &config, &block_with_secret());
        assert_eq!(
            fields(&planned)["password"],
            Value::String("hunter2".into()),
            "handler should receive the real write-only value"
        );
    }

    #[test]
    fn strip_recurses_into_nested_block() {
        let block = Block {
            attributes: vec![AttributeSchema {
                required: true,
                ..AttributeSchema::new("name", Type::String)
            }],
            nested_blocks: vec![NestedBlock {
                name: "auth".into(),
                nesting: NestingMode::List,
                block: block_with_secret(),
                min_items: 0,
                max_items: 0,
            }],
        };
        let mut v = obj(&[
            ("name", Value::String("a".into())),
            (
                "auth",
                Value::List(vec![obj(&[
                    ("name", Value::String("primary".into())),
                    ("password", Value::String("s3cr3t".into())),
                ])]),
            ),
        ]);
        strip(&mut v, &block);
        let Value::List(items) = &fields(&v)["auth"] else {
            panic!("auth should be a list");
        };
        assert!(
            fields(&items[0])["password"].is_null(),
            "write-only inside a block is nulled too"
        );
    }
}
