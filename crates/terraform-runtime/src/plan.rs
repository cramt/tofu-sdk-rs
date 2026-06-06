//! The planning engine.
//!
//! Terraform calls `PlanResourceChange` with the prior state and a *proposed new
//! state* (config merged over prior). The provider turns that into a planned
//! state and tells Terraform which attribute changes force the resource to be
//! replaced rather than updated in place.
//!
//! This implementation:
//!
//! - returns a null plan for a null proposal (a destroy);
//! - emits a `requires_replace` path for every `force_new` attribute whose value
//!   changed (only on update — a create never replaces);
//! - marks computed attributes unknown when they are absent (null) in the
//!   proposal, and marks *all* computed attributes unknown when the resource is
//!   being replaced (it is effectively a fresh object) — recursing into nested
//!   blocks so a computed attribute inside a block is handled too.

use terraform_ir::{Block, NestingMode};
use terraform_tfplugin6::tfplugin6::{
    attribute_path::{step::Selector, Step},
    AttributePath,
};
use terraform_value::Value;

use crate::resource::PlanModifications;

/// The outcome of planning a single resource change.
pub struct Plan {
    /// The planned new state.
    pub planned: Value,
    /// Attribute paths whose change forces replacement.
    pub requires_replace: Vec<AttributePath>,
}

/// Plan a resource change from `prior` to `proposed` under `block`.
pub fn plan(prior: &Value, proposed: Value, block: &Block) -> Plan {
    // A null proposal is a destroy: nothing to plan, nothing to replace.
    if proposed.is_null() {
        return Plan {
            planned: Value::Null,
            requires_replace: Vec::new(),
        };
    }

    let requires_replace = requires_replace(prior, &proposed, block);
    let replacing = !requires_replace.is_empty();
    let planned = mark_computed_unknown(proposed, block, replacing);

    Plan {
        planned,
        requires_replace,
    }
}

/// Compute the attribute paths that force replacement: `force_new` attributes
/// whose value differs from prior. Only meaningful on update (a create — null
/// prior — never replaces).
fn requires_replace(prior: &Value, proposed: &Value, block: &Block) -> Vec<AttributePath> {
    let (Value::Object(prior_fields), Value::Object(proposed_fields)) = (prior, proposed) else {
        return Vec::new();
    };

    let mut paths = Vec::new();
    for attr in &block.attributes {
        if !attr.force_new {
            continue;
        }
        let before = prior_fields.get(&attr.name);
        let after = proposed_fields.get(&attr.name);
        if before != after {
            paths.push(attribute_path(&attr.name));
        }
    }
    paths
}

/// Apply attribute defaults and mark computed attributes unknown.
///
/// For each non-required attribute left unset (null): a declared `default` fills
/// it (and wins — a default is a known value); otherwise a computed attribute is
/// marked unknown. Computed attributes are *always* marked unknown when
/// `replacing` (the object is effectively fresh). Recurses into nested blocks so
/// defaults and computed attributes *inside* a block are handled too (otherwise a
/// computed-in-block applied value would trip Terraform's "inconsistent result
/// after apply" against a known-null plan).
fn mark_computed_unknown(proposed: Value, block: &Block, replacing: bool) -> Value {
    let Value::Object(mut fields) = proposed else {
        return proposed;
    };
    for attr in &block.attributes {
        if attr.required {
            continue;
        }
        let is_null = matches!(fields.get(&attr.name), Some(Value::Null) | None);
        // A default fills an unset optional attribute (and takes precedence over
        // the computed-unknown marking below — a defaulted value is known).
        if is_null {
            if let Some(default) = &attr.default {
                fields.insert(attr.name.clone(), default.clone());
                continue;
            }
        }
        if attr.computed && (replacing || is_null) {
            fields.insert(attr.name.clone(), Value::Unknown);
        }
    }
    for nested in &block.nested_blocks {
        if let Some(value) = fields.remove(&nested.name) {
            fields.insert(
                nested.name.clone(),
                mark_block_computed_unknown(value, &nested.block, nested.nesting, replacing),
            );
        }
    }
    Value::Object(fields)
}

/// Recurse [`mark_computed_unknown`] into a nested block's value, walking each
/// element per its nesting mode (a single object, or the objects of a
/// list/set/map). Non-object shapes (null/unknown) pass through untouched.
fn mark_block_computed_unknown(
    value: Value,
    block: &Block,
    nesting: NestingMode,
    replacing: bool,
) -> Value {
    let element = |v: Value| mark_computed_unknown(v, block, replacing);
    match nesting {
        NestingMode::Single => element(value),
        NestingMode::List => match value {
            Value::List(items) => Value::List(items.into_iter().map(element).collect()),
            other => other,
        },
        NestingMode::Set => match value {
            Value::Set(items) => Value::Set(items.into_iter().map(element).collect()),
            other => other,
        },
        NestingMode::Map => match value {
            Value::Map(entries) => {
                Value::Map(entries.into_iter().map(|(k, v)| (k, element(v))).collect())
            }
            other => other,
        },
    }
}

/// Apply a resource's [`PlanModifications`] to the mechanically-produced plan:
/// mark named top-level attributes unknown and add `require_replace` paths
/// (deduped against the mechanical ones).
pub fn apply_modifications(plan: &mut Plan, mods: PlanModifications) {
    if let Value::Object(fields) = &mut plan.planned {
        for name in &mods.unknown {
            if fields.contains_key(name) {
                fields.insert(name.clone(), Value::Unknown);
            }
        }
    }
    for name in mods.require_replace {
        let path = attribute_path(&name);
        if !plan.requires_replace.contains(&path) {
            plan.requires_replace.push(path);
        }
    }
}

/// A single-step path to a top-level attribute.
fn attribute_path(name: &str) -> AttributePath {
    AttributePath {
        steps: vec![Step {
            selector: Some(Selector::AttributeName(name.to_string())),
        }],
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use terraform_ir::{AttributeSchema, Block, NestedBlock, NestingMode};
    use terraform_value::{Type, Value};

    use super::*;

    fn block() -> Block {
        Block {
            attributes: vec![
                AttributeSchema {
                    force_new: true,
                    required: true,
                    ..AttributeSchema::new("name", Type::String)
                },
                AttributeSchema {
                    computed: true,
                    ..AttributeSchema::new("arn", Type::String)
                },
                AttributeSchema {
                    optional: true,
                    ..AttributeSchema::new("note", Type::String)
                },
            ],
            nested_blocks: Vec::new(),
        }
    }

    fn obj(pairs: &[(&str, Value)]) -> Value {
        let mut m = BTreeMap::new();
        for (k, v) in pairs {
            m.insert(k.to_string(), v.clone());
        }
        Value::Object(m)
    }

    fn fields(v: &Value) -> &BTreeMap<String, Value> {
        match v {
            Value::Object(m) => m,
            _ => panic!("expected object"),
        }
    }

    #[test]
    fn create_marks_computed_unknown_no_replace() {
        let proposed = obj(&[
            ("name", Value::String("a".into())),
            ("arn", Value::Null),
            ("note", Value::Null),
        ]);
        let p = plan(&Value::Null, proposed, &block());
        assert!(p.requires_replace.is_empty(), "create never replaces");
        assert!(fields(&p.planned)["arn"].is_unknown());
        assert!(fields(&p.planned)["note"].is_null(), "optional stays null");
    }

    #[test]
    fn destroy_plans_null() {
        let p = plan(
            &obj(&[("name", Value::String("a".into()))]),
            Value::Null,
            &block(),
        );
        assert!(p.planned.is_null());
        assert!(p.requires_replace.is_empty());
    }

    #[test]
    fn force_new_change_requires_replace_and_recomputes() {
        let prior = obj(&[
            ("name", Value::String("a".into())),
            ("arn", Value::String("arn:a".into())),
            ("note", Value::Null),
        ]);
        // name changed; proposed carries prior computed arn forward.
        let proposed = obj(&[
            ("name", Value::String("b".into())),
            ("arn", Value::String("arn:a".into())),
            ("note", Value::Null),
        ]);
        let p = plan(&prior, proposed, &block());
        assert_eq!(p.requires_replace.len(), 1, "name forces replacement");
        // Replacement => computed becomes unknown even though proposal had a value.
        assert!(fields(&p.planned)["arn"].is_unknown());
    }

    /// A block (`settings`, single) with a computed `id` inside it.
    fn block_with_nested() -> Block {
        Block {
            attributes: vec![AttributeSchema {
                force_new: true,
                required: true,
                ..AttributeSchema::new("name", Type::String)
            }],
            nested_blocks: vec![NestedBlock {
                name: "settings".into(),
                nesting: NestingMode::List,
                block: Block {
                    attributes: vec![
                        AttributeSchema {
                            required: true,
                            ..AttributeSchema::new("key", Type::String)
                        },
                        AttributeSchema {
                            computed: true,
                            ..AttributeSchema::new("id", Type::String)
                        },
                    ],
                    nested_blocks: Vec::new(),
                },
            }],
        }
    }

    #[test]
    fn default_applies_to_unset_optional_attribute() {
        let block = Block {
            attributes: vec![
                AttributeSchema {
                    required: true,
                    ..AttributeSchema::new("name", Type::String)
                },
                AttributeSchema {
                    optional: true,
                    default: Some(Value::String("us-east-1".into())),
                    ..AttributeSchema::new("region", Type::String)
                },
            ],
            nested_blocks: Vec::new(),
        };

        // Unset (null) -> default applies.
        let p = plan(
            &Value::Null,
            obj(&[("name", Value::String("a".into())), ("region", Value::Null)]),
            &block,
        );
        assert_eq!(
            fields(&p.planned)["region"],
            Value::String("us-east-1".into())
        );

        // Set by the user -> the user's value wins over the default.
        let p = plan(
            &Value::Null,
            obj(&[
                ("name", Value::String("a".into())),
                ("region", Value::String("eu-west-1".into())),
            ]),
            &block,
        );
        assert_eq!(
            fields(&p.planned)["region"],
            Value::String("eu-west-1".into())
        );
    }

    #[test]
    fn default_wins_over_computed_unknown() {
        // An optional+computed attribute with a default takes the default (a known
        // value) rather than going unknown.
        let block = Block {
            attributes: vec![AttributeSchema {
                optional: true,
                computed: true,
                default: Some(Value::number(5)),
                ..AttributeSchema::new("size", Type::Number)
            }],
            nested_blocks: Vec::new(),
        };
        let p = plan(&Value::Null, obj(&[("size", Value::Null)]), &block);
        assert_eq!(fields(&p.planned)["size"], Value::number(5));
    }

    #[test]
    fn computed_attr_inside_block_marked_unknown_on_create() {
        let settings = Value::List(vec![obj(&[
            ("key", Value::String("k".into())),
            ("id", Value::Null),
        ])]);
        let proposed = obj(&[("name", Value::String("a".into())), ("settings", settings)]);

        let p = plan(&Value::Null, proposed, &block_with_nested());
        let Value::List(items) = &fields(&p.planned)["settings"] else {
            panic!("settings should be a list");
        };
        assert!(
            fields(&items[0])["id"].is_unknown(),
            "computed `id` inside the block should be planned unknown"
        );
        assert_eq!(fields(&items[0])["key"], Value::String("k".into()));
    }

    #[test]
    fn computed_attr_inside_block_unknown_on_replace() {
        // name (force_new) changes -> replacing -> even a *known* computed-in-block
        // value goes unknown.
        let inside = |id: &str| {
            Value::List(vec![obj(&[
                ("key", Value::String("k".into())),
                ("id", Value::String(id.into())),
            ])])
        };
        let prior = obj(&[
            ("name", Value::String("a".into())),
            ("settings", inside("old")),
        ]);
        let proposed = obj(&[
            ("name", Value::String("b".into())),
            ("settings", inside("old")),
        ]);

        let p = plan(&prior, proposed, &block_with_nested());
        assert_eq!(p.requires_replace.len(), 1, "name forces replacement");
        let Value::List(items) = &fields(&p.planned)["settings"] else {
            panic!("settings should be a list");
        };
        assert!(
            fields(&items[0])["id"].is_unknown(),
            "on replace, computed-in-block goes unknown even with a prior value"
        );
    }

    #[test]
    fn in_place_update_keeps_computed() {
        let prior = obj(&[
            ("name", Value::String("a".into())),
            ("arn", Value::String("arn:a".into())),
            ("note", Value::String("old".into())),
        ]);
        // Only the non-force-new `note` changed.
        let proposed = obj(&[
            ("name", Value::String("a".into())),
            ("arn", Value::String("arn:a".into())),
            ("note", Value::String("new".into())),
        ]);
        let p = plan(&prior, proposed, &block());
        assert!(p.requires_replace.is_empty(), "no force_new change");
        assert_eq!(
            fields(&p.planned)["arn"],
            Value::String("arn:a".into()),
            "computed value preserved on in-place update"
        );
    }
}
