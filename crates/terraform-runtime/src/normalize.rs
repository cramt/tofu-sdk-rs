//! Semantic-equality spike — diff suppression derived from the type, not a hook.
//!
//! Terraform core, not the provider, computes the user-facing diff: it compares
//! the prior state and the planned state structurally over `cty` values. So a
//! provider that wants two differently-encoded-but-equal values (a
//! case-insensitive ID, a normalized URL, an ARN whose casing the API echoes back
//! differently) to register as *no change* cannot supply a custom equality
//! function — its only lever is `PlanResourceChange`, where it returns the planned
//! state. The blessed move (what SDKv2's `DiffSuppressFunc` and the Plugin
//! Framework's `StringSemanticEquals` both do under the hood) is: when the new
//! value is *semantically equal* to the prior, plan the **prior value verbatim**,
//! so core's structural compare then yields no-op.
//!
//! The thesis this spike tests: in a properly typed SDK the author should never
//! write that equality function. If a domain value is modeled as a *quotient
//! type* — a newtype whose constructor maps every member of an equivalence class
//! to one canonical representative (`Arn`, `CaseInsensitive`, …) — then "semantic
//! equality" is just `canonical(a) == canonical(b)`, and the canonicalizer is the
//! type's own constructor. "Parse, don't validate," but the parse quotients.
//!
//! ## How the quotient is expressed (spike findings on facet 0.46.5)
//!
//! The clean author mechanism is facet's **`#[facet(opaque, proxy = String)]`**:
//! the newtype serializes through a `String` proxy via *bidirectional `TryFrom`*
//! (`TryFrom<String> for T` = the canonicalizing parse; `TryFrom<&T> for String` =
//! the canonical render). Those two conversions ARE the quotient — and
//! [`string_quotient`] turns them into a value-level canonicalizer with no
//! per-resource code. The same conversions facet uses to (de)serialize the type
//! are reused as its diff semantics.
//!
//! What does *not* work yet, and is the integration TODO:
//! - `#[derive(Facet)]` auto-wires the `display`/`debug`/`partial_eq` vtable hooks
//!   (via spez) but has **no `parse` arm** — a `FromStr` impl never reaches the
//!   `parse` vtable slot. So the "harvest `parse`+`display` off any type's vtable"
//!   route only works for facet *builtins* that wire it explicitly
//!   (`vtable_direct!(T => FromStr, Display, …)`: `Ipv6Addr`, `Url`, …).
//! - `terraform-codec` does not drive the `try_from` vtable, so an `opaque+proxy`
//!   type does not round-trip through the codec yet. Auto-*harvesting* the
//!   canonicalizer by reflection (walk `M::SHAPE`, detect proxy/quotient fields,
//!   build a [`Canon`] automatically) therefore needs that codec bridge first.
//!   Until then a [`Canon`] is assembled explicitly from [`string_quotient`].
//!
//! ## Scope of the spike
//!
//! - **Top-level scalar attributes only** (nested blocks/collections are a
//!   follow-up — the pre-pass would recurse like `write_only::strip`).
//! - The pre-pass [`keep_prior`] runs *before* [`crate::plan::plan`]: rewriting a
//!   semantically-unchanged proposed value back to the prior means the mechanical
//!   planner then sees `before == after` and emits neither a spurious
//!   `requires_replace` nor a spurious diff. One transform; everything downstream
//!   stays correct.
//! - This is the **store-raw, normalize-on-compare** variant: state keeps exactly
//!   what the user wrote (we keep the *prior* bytes), so we never trip Terraform's
//!   "provider produced inconsistent result" check by planning a third value.
//! - The irreducible residue — *server-authoritative* normalization, where only
//!   the remote knows the canonical form — is out of scope; no client-side parse
//!   can reproduce it. That still needs `modify_plan`.

use std::collections::BTreeMap;
use std::convert::TryFrom;

use terraform_value::Value;

/// A value-level canonicalizer: maps a `Value` to its canonical representative.
/// For a quotient type it is `to_proxy(parse(v))`; for anything it never sees as a
/// quotient it is the identity.
type Canonicalizer = Box<dyn Fn(&Value) -> Value + Send + Sync>;

/// A map from a top-level attribute name to its canonicalizer, for every
/// attribute backed by a quotient type. Assembled explicitly for now (see the
/// module docs on why reflection auto-harvest needs a codec bridge first).
#[derive(Default)]
pub struct Canon {
    fields: BTreeMap<String, Canonicalizer>,
}

impl Canon {
    pub fn new() -> Self {
        Canon::default()
    }

    /// Register the canonicalizer for attribute `name`. Use [`string_quotient`]
    /// to derive one straight from a quotient type's proxy conversions.
    pub fn with(mut self, name: impl Into<String>, canon: Canonicalizer) -> Self {
        self.fields.insert(name.into(), canon);
        self
    }

    /// True when no attribute is a quotient — the planner skips the pre-pass
    /// entirely (zero overhead for the common case, like `write_only`'s `block_has`).
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

/// Build a canonicalizer from a string-backed quotient type `T` — exactly the two
/// conversions facet's `#[facet(opaque, proxy = String)]` requires:
/// `TryFrom<String> for T` (the canonicalizing parse) and `TryFrom<&T> for String`
/// (render the representative back). A string the type rejects passes through
/// unchanged (a validation concern handled elsewhere); non-strings pass through.
///
/// The author writes the quotient newtype once; the canonicalizer — and thus the
/// resource's semantic-equality diff behavior — falls out of the type. No
/// `DiffSuppressFunc`, no per-attribute closure at the call site.
pub fn string_quotient<T>() -> Canonicalizer
where
    T: TryFrom<String>,
    for<'a> String: TryFrom<&'a T>,
{
    Box::new(|value| match value {
        Value::String(s) => T::try_from(s.clone())
            .ok()
            .and_then(|t| String::try_from(&t).ok())
            .map(Value::String)
            .unwrap_or_else(|| value.clone()),
        other => other.clone(),
    })
}

/// Diff-suppression pre-pass: for each quotient-typed attribute present in both
/// `prior` and `proposed`, if the two are *semantically equal* (equal after
/// canonicalization) rewrite the proposed value back to the **prior** value
/// verbatim. Run before [`crate::plan::plan`]; the mechanical planner then sees an
/// unchanged attribute and emits neither a spurious diff nor a spurious
/// `requires_replace`.
///
/// A no-op on create (a null prior is not an object) and for any non-quotient
/// attribute.
pub fn keep_prior(prior: &Value, proposed: &mut Value, canon: &Canon) {
    if canon.is_empty() {
        return;
    }
    let Value::Object(prior_fields) = prior else {
        return;
    };
    let Value::Object(proposed_fields) = proposed else {
        return;
    };
    for (name, canonicalize) in &canon.fields {
        let Some(prior_value) = prior_fields.get(name) else {
            continue;
        };
        let Some(proposed_value) = proposed_fields.get(name) else {
            continue;
        };
        if canonicalize(prior_value) == canonicalize(proposed_value) {
            let kept = prior_value.clone();
            proposed_fields.insert(name.clone(), kept);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;

    // A case-insensitive identifier: the quotient is "same up to ASCII case".
    // `#[facet(opaque, proxy = String)]` is the author-facing declaration — it
    // makes facet (de)serialize the type through these very `TryFrom`s, so the
    // canonicalizer below and the codec see one source of truth. (We exercise the
    // conversions directly here; the codec-driven path is the integration TODO.)
    #[derive(facet::Facet)]
    #[facet(opaque, proxy = String)]
    struct CiId(String);

    impl TryFrom<String> for CiId {
        type Error = Infallible;
        fn try_from(s: String) -> Result<Self, Self::Error> {
            // The canonicalizing parse: lowercase is the representative.
            Ok(CiId(s.to_lowercase()))
        }
    }
    impl TryFrom<&CiId> for String {
        type Error = Infallible;
        fn try_from(id: &CiId) -> Result<Self, Self::Error> {
            Ok(id.0.clone())
        }
    }

    fn ci_canon() -> Canon {
        Canon::new().with("id", string_quotient::<CiId>())
    }

    #[test]
    fn quotient_canonicalizes_via_the_types_own_conversions() {
        let canon = string_quotient::<CiId>();
        assert_eq!(canon(&Value::String("AbC".into())), Value::String("abc".into()));
        // already canonical -> unchanged
        assert_eq!(canon(&Value::String("abc".into())), Value::String("abc".into()));
        // non-string -> identity
        assert_eq!(canon(&Value::Null), Value::Null);
    }

    #[test]
    fn keep_prior_suppresses_case_only_change() {
        let canon = ci_canon();
        // Prior stored the user's original casing.
        let prior = obj(&[("id", "aBc")]);
        // User re-typed the same id with different casing — semantically equal.
        let mut proposed = obj(&[("id", "ABC")]);

        keep_prior(&prior, &mut proposed, &canon);

        // Rewritten back to the prior bytes verbatim: a structural diff sees no
        // change, and state stays exactly as first written ("aBc") — never a
        // provider-invented third value.
        assert_eq!(field(&proposed, "id"), "aBc");
    }

    #[test]
    fn keep_prior_leaves_real_change_untouched() {
        let canon = ci_canon();
        let prior = obj(&[("id", "abc")]);
        let mut proposed = obj(&[("id", "xyz")]); // genuinely different

        keep_prior(&prior, &mut proposed, &canon);

        assert_eq!(field(&proposed, "id"), "xyz", "real change preserved");
    }

    #[test]
    fn keep_prior_never_touches_non_quotient_fields() {
        let canon = ci_canon(); // only "id" is a quotient
        let prior = obj(&[("id", "abc"), ("note", "OLD")]);
        let mut proposed = obj(&[("id", "abc"), ("note", "new")]);

        keep_prior(&prior, &mut proposed, &canon);

        assert_eq!(field(&proposed, "note"), "new", "non-quotient diff stays");
    }

    #[test]
    fn no_op_on_create() {
        let canon = ci_canon();
        let mut proposed = obj(&[("id", "ABC")]);
        // Null prior (create): nothing to keep, proposed is untouched.
        keep_prior(&Value::Null, &mut proposed, &canon);
        assert_eq!(field(&proposed, "id"), "ABC");
    }

    #[test]
    fn end_to_end_no_replace_for_case_only_change_on_force_new_attr() {
        use crate::plan::plan;
        use terraform_ir::{AttributeSchema, Block};
        use terraform_value::Type as CtyType;

        let block = Block {
            attributes: vec![AttributeSchema {
                force_new: true,
                required: true,
                ..AttributeSchema::new("id", CtyType::String)
            }],
            nested_blocks: Vec::new(),
        };
        let canon = ci_canon();

        let prior = obj(&[("id", "aBc")]);
        let mut proposed = obj(&[("id", "ABC")]);

        // Without the pre-pass: the force_new `id` "changed" → spurious replace.
        let naive = plan(&prior, proposed.clone(), &block);
        assert_eq!(
            naive.requires_replace.len(),
            1,
            "byte-diff alone would force a replacement"
        );

        // With the pre-pass: the case-only change is suppressed → no replacement.
        keep_prior(&prior, &mut proposed, &canon);
        let suppressed = plan(&prior, proposed, &block);
        assert!(
            suppressed.requires_replace.is_empty(),
            "semantic equality suppresses the spurious force_new replacement"
        );
    }

    // --- helpers ---

    fn obj(pairs: &[(&str, &str)]) -> Value {
        let mut m = BTreeMap::new();
        for (k, v) in pairs {
            m.insert(k.to_string(), Value::String((*v).to_string()));
        }
        Value::Object(m)
    }

    fn field<'a>(v: &'a Value, name: &str) -> &'a str {
        match v {
            Value::Object(m) => match m.get(name) {
                Some(Value::String(s)) => s,
                _ => panic!("field {name} not a string"),
            },
            _ => panic!("not an object"),
        }
    }
}
