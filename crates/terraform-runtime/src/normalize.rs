//! Semantic equality (roadmap 3.6) — diff suppression derived from the type, not
//! a hook. A [`Canon`] is auto-harvested from the model's `SHAPE` by reflection
//! ([`Canon::harvest`], the default behind [`crate::Resource::semantic_equality`]),
//! so a quotient field needs zero wiring; [`Canon::with`] + [`string_quotient`]
//! remain for explicit additions.
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
//! ## How the quotient is expressed (facet 0.46.5)
//!
//! The clean author mechanism is facet's **`#[facet(opaque, proxy = String)]`**:
//! the newtype serializes through a `String` proxy via *bidirectional `TryFrom`*
//! (`TryFrom<String> for T` = the canonicalizing parse; `TryFrom<&T> for String` =
//! the canonical render). Those two conversions ARE the quotient — and
//! [`string_quotient`] turns them into a value-level canonicalizer with no
//! per-resource code. The same conversions facet uses to (de)serialize the type
//! are reused as its diff semantics.
//!
//! Where this stands now:
//! - **The codec bridge exists.** `terraform-codec` drives facet's container-level
//!   proxy vtable (`convert_in`/`convert_out` via
//!   `begin_custom_deserialization_from_shape` / `custom_serialization_from_shape`),
//!   and `terraform-reflect` maps an `opaque+proxy` field to its proxy's cty type.
//!   So a quotient type round-trips through the codec and **can be a real model
//!   field** (decode runs the canonicalizing `TryFrom`, encode renders it back).
//! - **Auto-harvest is wired.** [`Canon::harvest`] walks `M::SHAPE`, detects each
//!   top-level quotient field (a container-proxy type, optionally `Option`-wrapped)
//!   and registers a canonicalizer built from
//!   [`terraform_codec::canonicalize_through_shape`] — the type-erased,
//!   `Value`-level round-trip (`Partial::alloc_shape` → fill → re-encode). It is the
//!   default behind [`crate::Resource::semantic_equality`], so a quotient field needs
//!   no per-resource code. The `ResourceAdapter` builds it once at construction and
//!   clones it per plan (no per-plan `SHAPE` walk). Recurses into single nested
//!   blocks; repeated (list/set) blocks are the remaining follow-up.
//! - One caveat unrelated to the codec: `#[derive(Facet)]` auto-wires the
//!   `display`/`debug`/`partial_eq` vtable hooks (via spez) but has **no `parse`
//!   arm** — a `FromStr` impl never reaches the `parse` vtable slot. The quotient
//!   route deliberately uses the `proxy` `TryFrom` conversions instead, which the
//!   derive *does* wire, so this does not affect [`string_quotient`].
//!
//! ## Scope (current)
//!
//! - **Top-level scalars and single nested blocks.** A quotient scalar at the top
//!   level, or inside a *single* nested struct/block (`Struct` / `Option<Struct>`,
//!   recursively), is suppressed. **Repeated** blocks (`Vec`/`HashSet` of struct —
//!   list/set nesting) are *not* recursed: keeping a quotient inside a reordered
//!   collection needs element matching, which is out of scope.
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
use std::sync::Arc;

use facet::{Def, Facet, Shape, Type as FType, UserType};
use terraform_value::Value;

/// A value-level canonicalizer: maps a `Value` to its canonical representative.
/// For a quotient type it is `to_proxy(parse(v))`; for anything it never sees as a
/// quotient it is the identity. `Arc` (not `Box`) so a [`Canon`] is cheap to clone
/// — the `ResourceAdapter` builds a resource's `Canon` once at construction and
/// hands out clones per plan, so this `Arc` is shared, never rebuilt.
type Canonicalizer = Arc<dyn Fn(&Value) -> Value + Send + Sync>;

/// The semantic-equality canonicalizers for a resource. `fields` maps a top-level
/// attribute name to its canonicalizer (for quotient-typed scalars); `blocks` maps a
/// **single** nested struct/block name to a child `Canon` for the quotients *inside*
/// it (recursively). Auto-harvested from a model's `SHAPE` via [`Canon::harvest`], or
/// assembled explicitly with [`Canon::with`] (top-level only).
///
/// Repeated blocks (`Vec`/`HashSet` of struct, i.e. list/set nesting) are **not**
/// recursed — suppressing a quotient inside a reordered collection needs element
/// matching, which is out of scope (see the module docs).
#[derive(Default, Clone)]
#[must_use = "a Canon must be returned from `Resource::semantic_equality` to take effect"]
pub struct Canon {
    fields: BTreeMap<String, Canonicalizer>,
    blocks: BTreeMap<String, Canon>,
}

impl Canon {
    pub fn new() -> Self {
        Canon::default()
    }

    /// Register the canonicalizer for attribute `name`. Use [`string_quotient`]
    /// to derive one straight from a quotient type's proxy conversions. Chainable;
    /// the returned `Canon` must be used (dropping it discards the registration —
    /// enforced by the type-level `#[must_use]`).
    pub fn with(mut self, name: impl Into<String>, canon: Canonicalizer) -> Self {
        self.fields.insert(name.into(), canon);
        self
    }

    /// Auto-harvest a `Canon` from a model's reflected `SHAPE`: every top-level
    /// field whose type is a quotient — a container-`proxy` type (e.g.
    /// `#[facet(opaque, proxy = String)]`), optionally wrapped in `Option` — gets a
    /// canonicalizer derived from its proxy conversions, with **no per-resource
    /// wiring**. This is the zero-config counterpart to building a `Canon` by hand
    /// with [`Canon::with`] + [`string_quotient`], and the default behind
    /// [`Resource::semantic_equality`](crate::Resource::semantic_equality).
    ///
    /// A model with no quotient fields harvests an empty `Canon` (the planner then
    /// skips the pre-pass entirely — zero overhead). Recurses into single nested
    /// blocks; repeated (list/set) blocks are not, matching [`keep_prior`]'s scope.
    pub fn harvest<M: Facet<'static>>() -> Self {
        Canon::harvest_shape(M::SHAPE)
    }

    fn harvest_shape(shape: &'static Shape) -> Self {
        let mut canon = Canon::new();
        let FType::User(UserType::Struct(st)) = &shape.ty else {
            return canon;
        };
        for field in st.fields {
            let fshape = field.shape();
            let name = field.rename.unwrap_or(field.name);
            if quotient_inner(fshape).is_some() {
                // A quotient scalar: canonicalize through the field's *own* shape
                // (incl. any `Option`), so null and present values both compose.
                canon.fields.insert(
                    name.to_string(),
                    Arc::new(move |v: &Value| {
                        terraform_codec::canonicalize_through_shape(fshape, v)
                    }),
                );
            } else if let Some(inner) = single_struct_inner(fshape) {
                // A single nested struct/block: recurse, keeping it only if it holds
                // a quotient somewhere inside.
                let nested = Canon::harvest_shape(inner);
                if !nested.is_empty() {
                    canon.blocks.insert(name.to_string(), nested);
                }
            }
        }
        canon
    }

    /// True when neither a top-level attribute nor any nested block carries a
    /// quotient — the planner skips the pre-pass entirely (zero overhead for the
    /// common case, like `write_only`'s `block_has`).
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty() && self.blocks.is_empty()
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
    Arc::new(|value| match value {
        Value::String(s) => T::try_from(s.clone())
            .ok()
            .and_then(|t| String::try_from(&t).ok())
            .map(Value::String)
            .unwrap_or_else(|| value.clone()),
        other => other.clone(),
    })
}

/// Peel `Option` wrappers and report whether the underlying type is a container
/// proxy (a quotient). Returns the proxy-bearing shape if so, else `None` — used by
/// [`Canon::harvest`] to decide which fields carry a canonicalizer.
fn quotient_inner(shape: &'static Shape) -> Option<&'static Shape> {
    if shape.has_any_proxy() {
        return Some(shape);
    }
    match shape.def {
        Def::Option(opt) => quotient_inner(opt.t),
        _ => None,
    }
}

/// Peel `Option` and return the inner shape iff it is a **single** nested struct
/// (not a list/set/map of structs, nor a scalar) — the case [`keep_prior`] can
/// recurse into as one object. Returns `None` for collections (element matching is
/// out of scope) and non-structs.
fn single_struct_inner(shape: &'static Shape) -> Option<&'static Shape> {
    match shape.def {
        Def::Option(opt) => single_struct_inner(opt.t),
        Def::List(_) | Def::Slice(_) | Def::Array(_) | Def::Set(_) | Def::Map(_) => None,
        _ => matches!(&shape.ty, FType::User(UserType::Struct(_))).then_some(shape),
    }
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
    // Null/unknown prior (create, or a not-yet-known object) → nothing to keep.
    // A field-level `Unknown` inside the prior object never canonicalizes equal to
    // a concrete proposed string (the `string_quotient` closure passes `Unknown`
    // through unchanged), so an unknown prior field is never suppressed — correct,
    // since a configured resource's applied state holds concrete values, not
    // unknowns.
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

    // Recurse into single nested blocks: keep any quotient *inside* the block that is
    // semantically equal, leaving the rest of the block (and the block's presence)
    // exactly as proposed. A null/absent block on either side has nothing to keep —
    // the recursive call's own object guards handle that.
    for (name, nested) in &canon.blocks {
        let Some(prior_block) = prior_fields.get(name) else {
            continue;
        };
        let Some(proposed_block) = proposed_fields.get_mut(name) else {
            continue;
        };
        keep_prior(prior_block, proposed_block, nested);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;

    // A case-insensitive identifier: the quotient is "same up to ASCII case".
    // `#[facet(opaque, proxy = String)]` is the author-facing declaration — it
    // makes facet (de)serialize the type through these very `TryFrom`s, so the
    // canonicalizer below and the codec see one source of truth. (The codec-driven
    // path now works too — see `terraform-codec`'s proxy round-trip tests; here we
    // exercise the conversions directly.)
    #[derive(facet::Facet)]
    #[facet(opaque, proxy = String)]
    struct CiId(String);

    // facet's `opaque, proxy` contract mandates `TryFrom` in both directions even
    // when a particular quotient's conversions can't fail, so the infallible-case
    // lint is a false positive here.
    #[allow(clippy::infallible_try_from)]
    impl TryFrom<String> for CiId {
        type Error = Infallible;
        fn try_from(s: String) -> Result<Self, Self::Error> {
            // The canonicalizing parse: lowercase is the representative.
            Ok(CiId(s.to_lowercase()))
        }
    }
    #[allow(clippy::infallible_try_from)]
    impl TryFrom<&CiId> for String {
        type Error = Infallible;
        fn try_from(id: &CiId) -> Result<Self, Self::Error> {
            Ok(id.0.clone())
        }
    }

    fn ci_canon() -> Canon {
        Canon::new().with("id", string_quotient::<CiId>())
    }

    // A model with a quotient field (bare and Option-wrapped) plus a plain field —
    // used to prove `Canon::harvest` finds exactly the quotient attributes.
    #[derive(facet::Facet)]
    #[allow(dead_code)]
    struct Model {
        name: String,
        id: CiId,
        alias: Option<CiId>,
    }

    #[derive(facet::Facet)]
    #[allow(dead_code)]
    struct PlainModel {
        name: String,
        count: i64,
    }

    #[test]
    fn harvest_registers_only_quotient_fields() {
        let canon = Canon::harvest::<Model>();
        assert!(!canon.is_empty());
        // Both the bare and the Option-wrapped quotient canonicalize to lowercase…
        assert_eq!(
            canon.fields["id"](&Value::String("AbC".into())),
            Value::String("abc".into())
        );
        assert_eq!(
            canon.fields["alias"](&Value::String("XyZ".into())),
            Value::String("xyz".into())
        );
        // …and a null Option stays null.
        assert_eq!(canon.fields["alias"](&Value::Null), Value::Null);
        // The plain field is not registered.
        assert!(!canon.fields.contains_key("name"));
    }

    #[test]
    fn harvest_of_a_model_without_quotients_is_empty() {
        let canon = Canon::harvest::<PlainModel>();
        assert!(
            canon.is_empty(),
            "no quotient fields -> empty Canon -> no pre-pass"
        );
    }

    #[test]
    fn harvested_canon_suppresses_case_only_change() {
        // End-to-end through the harvested Canon (no hand wiring): a case-only
        // change to a quotient field is kept as the prior value.
        let canon = Canon::harvest::<Model>();
        let prior = obj(&[("id", "aBc")]);
        let mut proposed = obj(&[("id", "ABC")]);
        keep_prior(&prior, &mut proposed, &canon);
        assert_eq!(field(&proposed, "id"), "aBc");
    }

    // A single nested block holding a quotient, plus a repeated one that must NOT be
    // recursed (collection element-matching is out of scope).
    #[derive(facet::Facet)]
    #[allow(dead_code)]
    struct Settings {
        id: CiId,
        note: String,
    }

    #[derive(facet::Facet)]
    #[allow(dead_code)]
    struct Nested {
        name: String,
        settings: Settings,
        maybe: Option<Settings>,
        list: Vec<Settings>,
    }

    #[test]
    fn harvest_recurses_into_single_nested_blocks_only() {
        let canon = Canon::harvest::<Nested>();
        assert!(!canon.is_empty());
        assert!(canon.blocks.contains_key("settings"), "bare single block");
        assert!(canon.blocks.contains_key("maybe"), "optional single block");
        assert!(
            !canon.blocks.contains_key("list"),
            "repeated blocks are not recursed"
        );
        // The block's own quotient is registered one level down.
        assert!(canon.blocks["settings"].fields.contains_key("id"));
        assert!(!canon.fields.contains_key("name"));
    }

    #[test]
    fn keep_prior_suppresses_case_only_change_inside_a_block() {
        let canon = Canon::harvest::<Nested>();
        let prior = nested_obj("aBc", "keep");
        let mut proposed = nested_obj("ABC", "keep"); // case-only change to inner id
        keep_prior(&prior, &mut proposed, &canon);
        assert_eq!(
            inner_field(&proposed, "settings", "id"),
            "aBc",
            "case-only inner change kept the prior bytes"
        );
    }

    #[test]
    fn keep_prior_preserves_real_change_inside_a_block() {
        let canon = Canon::harvest::<Nested>();
        let prior = nested_obj("abc", "old");
        let mut proposed = nested_obj("xyz", "new"); // real id change + note change
        keep_prior(&prior, &mut proposed, &canon);
        assert_eq!(inner_field(&proposed, "settings", "id"), "xyz");
        assert_eq!(
            inner_field(&proposed, "settings", "note"),
            "new",
            "non-quotient inner change is untouched"
        );
    }

    #[test]
    fn quotient_canonicalizes_via_the_types_own_conversions() {
        let canon = string_quotient::<CiId>();
        assert_eq!(
            canon(&Value::String("AbC".into())),
            Value::String("abc".into())
        );
        // already canonical -> unchanged
        assert_eq!(
            canon(&Value::String("abc".into())),
            Value::String("abc".into())
        );
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

    /// Build `{ name: "n", settings: { id, note } }` for the nested-block tests.
    fn nested_obj(id: &str, note: &str) -> Value {
        let mut settings = BTreeMap::new();
        settings.insert("id".to_string(), Value::String(id.to_string()));
        settings.insert("note".to_string(), Value::String(note.to_string()));
        let mut outer = BTreeMap::new();
        outer.insert("name".to_string(), Value::String("n".to_string()));
        outer.insert("settings".to_string(), Value::Object(settings));
        Value::Object(outer)
    }

    /// Read `v[block][name]` as a string.
    fn inner_field<'a>(v: &'a Value, block: &str, name: &str) -> &'a str {
        match v {
            Value::Object(m) => field(m.get(block).expect("block present"), name),
            _ => panic!("not an object"),
        }
    }
}
