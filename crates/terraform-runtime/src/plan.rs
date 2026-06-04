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
//!   being replaced (it is effectively a fresh object).

use terraform_ir::Block;
use terraform_tfplugin6::tfplugin6::{
    attribute_path::{step::Selector, Step},
    AttributePath,
};
use terraform_value::Value;

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

/// Mark computed attributes unknown: always when `replacing`, otherwise only
/// those left null by the caller.
fn mark_computed_unknown(proposed: Value, block: &Block, replacing: bool) -> Value {
    let Value::Object(mut fields) = proposed else {
        return proposed;
    };
    for attr in &block.attributes {
        if !attr.computed || attr.required {
            continue;
        }
        let is_null = matches!(fields.get(&attr.name), Some(Value::Null));
        if replacing || is_null {
            fields.insert(attr.name.clone(), Value::Unknown);
        }
    }
    Value::Object(fields)
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

    use terraform_ir::{AttributeSchema, Block};
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
