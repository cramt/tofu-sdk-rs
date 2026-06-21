//! The author-facing [`Resource`] trait and its internal type erasure.
//!
//! Authors implement [`Resource`] over a plain Rust `Model` type; the runtime
//! wraps each handler in a [`ResourceAdapter`] that decodes the dynamic
//! [`Value`] from Terraform into the model, calls the typed method, and encodes
//! the result back. The erased [`DynResource`] is what the gRPC service stores
//! and dispatches to.

use std::sync::Arc;

use async_trait::async_trait;
use facet::Facet;
use terraform_codec::{from_value, to_value};
use terraform_value::Value;

use crate::normalize::Canon;

use crate::ctx::{current_ctx, Ctx};

/// An error returned by a resource operation, surfaced to Terraform as an error
/// diagnostic.
///
/// Beyond a `summary`/`detail`, the error can be pointed at a specific attribute
/// with [`ResourceError::at`] and can carry accompanying warning diagnostics with
/// [`ResourceError::with_warning`] — both surface to Terraform alongside the
/// failure.
#[derive(Debug, Clone)]
pub struct ResourceError {
    /// Short, one-line summary.
    pub summary: String,
    /// Optional longer explanation.
    pub detail: String,
    /// Optional path to the offending attribute, as a sequence of attribute
    /// names (e.g. `["network", "subnet"]`). Empty means resource-wide.
    pub attribute: Vec<String>,
    /// Warning diagnostics to surface alongside the error.
    pub warnings: Vec<Diag>,
}

impl ResourceError {
    /// Create an error with a summary.
    pub fn new(summary: impl Into<String>) -> Self {
        ResourceError {
            summary: summary.into(),
            detail: String::new(),
            attribute: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// Attach a longer detail message.
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    /// Point this error at an attribute path (a sequence of names).
    pub fn at<I, S>(mut self, attribute: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.attribute = attribute.into_iter().map(Into::into).collect();
        self
    }

    /// Attach a warning diagnostic to surface alongside this error.
    pub fn with_warning(mut self, warning: Diag) -> Self {
        self.warnings.push(warning);
        self
    }
}

impl From<&str> for ResourceError {
    fn from(s: &str) -> Self {
        ResourceError::new(s)
    }
}

impl From<String> for ResourceError {
    fn from(s: String) -> Self {
        ResourceError::new(s)
    }
}

/// Severity of a [`Diag`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// A fatal problem; the operation failed.
    Error,
    /// A non-fatal advisory (e.g. via [`ResourceError::with_warning`] or
    /// [`Resource::validate`]).
    Warning,
}

/// A diagnostic message returned from an RPC.
#[derive(Debug, Clone)]
pub struct Diag {
    /// How severe the diagnostic is.
    pub severity: Severity,
    /// Short, one-line summary.
    pub summary: String,
    /// Optional longer explanation.
    pub detail: String,
    /// Optional path to the offending attribute, as a sequence of attribute
    /// names (e.g. `["network", "subnet"]`). Empty means provider-wide.
    pub attribute: Vec<String>,
}

impl Diag {
    /// An error diagnostic.
    pub fn error(summary: impl Into<String>, detail: impl Into<String>) -> Self {
        Diag {
            severity: Severity::Error,
            summary: summary.into(),
            detail: detail.into(),
            attribute: Vec::new(),
        }
    }

    /// A warning diagnostic (non-fatal advisory).
    pub fn warning(summary: impl Into<String>, detail: impl Into<String>) -> Self {
        Diag {
            severity: Severity::Warning,
            summary: summary.into(),
            detail: detail.into(),
            attribute: Vec::new(),
        }
    }

    /// Point this diagnostic at an attribute path (a sequence of names).
    pub fn at<I, S>(mut self, attribute: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.attribute = attribute.into_iter().map(Into::into).collect();
        self
    }
}

impl From<ResourceError> for Diag {
    fn from(e: ResourceError) -> Self {
        Diag {
            severity: Severity::Error,
            summary: e.summary,
            detail: e.detail,
            attribute: e.attribute,
        }
    }
}

impl From<ResourceError> for Diagnostics {
    /// The error diagnostic first, then any attached warnings.
    fn from(e: ResourceError) -> Self {
        let ResourceError {
            summary,
            detail,
            attribute,
            warnings,
        } = e;
        let mut diags = Vec::with_capacity(1 + warnings.len());
        diags.push(Diag {
            severity: Severity::Error,
            summary,
            detail,
            attribute,
        });
        diags.extend(warnings);
        diags
    }
}

/// A collection of diagnostics returned from an erased resource operation.
pub type Diagnostics = Vec<Diag>;

/// One step in an attribute [`Path`]: into an object attribute / block field by
/// name, into a list or set element by index, or into a map entry by key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathStep {
    /// Descend into an object attribute or block field by name.
    Attribute(String),
    /// Descend into a list or set element by index.
    Index(i64),
    /// Descend into a map entry by key.
    Key(String),
}

/// A path to a (possibly nested) attribute within a resource's planned value — a
/// sequence of [`PathStep`]s from the root object. A bare attribute name (via
/// `From<&str>`/`From<String>`) is the common single-step top-level case, so
/// `"tier"` and `Path::root().attribute("tier")` are equivalent; build deeper
/// paths to reach inside nested blocks (`settings[0].id`) or collections.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Path(pub Vec<PathStep>);

impl Path {
    /// An empty path (the root object). Extend it with
    /// [`attribute`](Path::attribute) / [`index`](Path::index) /
    /// [`key`](Path::key).
    pub fn root() -> Self {
        Self(Vec::new())
    }

    /// Descend into an object attribute or block field by name.
    pub fn attribute(mut self, name: impl Into<String>) -> Self {
        self.0.push(PathStep::Attribute(name.into()));
        self
    }

    /// Descend into a list or set element by index.
    pub fn index(mut self, index: i64) -> Self {
        self.0.push(PathStep::Index(index));
        self
    }

    /// Descend into a map entry by key.
    pub fn key(mut self, key: impl Into<String>) -> Self {
        self.0.push(PathStep::Key(key.into()));
        self
    }
}

impl From<&str> for Path {
    fn from(name: &str) -> Self {
        Path::root().attribute(name)
    }
}

impl From<String> for Path {
    fn from(name: String) -> Self {
        Path::root().attribute(name)
    }
}

/// Adjustments a resource's [`Resource::modify_plan`] makes to the
/// mechanically-produced plan: extra attributes to force replacement, and
/// attributes to mark unknown by custom rule. Each targets an attribute
/// [`Path`], so a nested attribute (inside a block, list, set, or map) can be
/// reached — a bare name still works for the common top-level case.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanModifications {
    /// Attribute paths whose planned change should force replacement, in addition
    /// to the `force_new` attributes handled mechanically.
    pub require_replace: Vec<Path>,
    /// Attribute paths to mark unknown in the planned state (a value the provider
    /// will compute by rule during apply).
    pub unknown: Vec<Path>,
}

impl PlanModifications {
    /// No modifications (the default).
    pub fn new() -> Self {
        Self::default()
    }

    /// Force replacement when the attribute at `path` changes. A bare name
    /// targets a top-level attribute; build a [`Path`] to target a nested one.
    pub fn require_replace(mut self, path: impl Into<Path>) -> Self {
        self.require_replace.push(path.into());
        self
    }

    /// Mark the attribute at `path` unknown in the plan. A bare name targets a
    /// top-level attribute; build a [`Path`] to target a nested one.
    pub fn unknown(mut self, path: impl Into<Path>) -> Self {
        self.unknown.push(path.into());
        self
    }
}

/// A managed resource type.
///
/// Implement this over a `Model` struct that reflects (via `#[derive(Facet)]`)
/// the resource's schema. `create` is required; `read` defaults to returning the
/// state unchanged, `update` to an error, and `delete` to a no-op.
#[async_trait]
pub trait Resource: Send + Sync + 'static {
    /// The Rust type modeling this resource's schema (config + computed state).
    type Model: Facet<'static> + Send + Sync;

    /// The current state-schema version. Bump it when a schema change requires
    /// migrating stored state, and implement [`upgrade`](Resource::upgrade).
    const SCHEMA_VERSION: i64 = 0;

    /// Create the resource from its planned state and return the new state with
    /// computed attributes filled in. Use `ctx` to emit success warnings, persist
    /// private state, or observe cancellation.
    async fn create(
        &self,
        ctx: &mut Ctx,
        planned: Self::Model,
    ) -> Result<Self::Model, ResourceError>;

    /// Refresh `current` from the real system. Return `None` if it no longer
    /// exists (Terraform will plan to recreate it). Defaults to returning
    /// `current` unchanged.
    async fn read(
        &self,
        _ctx: &mut Ctx,
        current: Self::Model,
    ) -> Result<Option<Self::Model>, ResourceError> {
        Ok(Some(current))
    }

    /// Update an existing resource to its planned state. Defaults to an error.
    async fn update(
        &self,
        _ctx: &mut Ctx,
        _planned: Self::Model,
        _prior: Self::Model,
    ) -> Result<Self::Model, ResourceError> {
        Err(ResourceError::new(
            "this resource does not support in-place update",
        ))
    }

    /// Delete the resource. Defaults to a no-op.
    async fn delete(&self, _ctx: &mut Ctx, _prior: Self::Model) -> Result<(), ResourceError> {
        Ok(())
    }

    /// Import an existing resource by its ID, returning the imported state.
    /// Terraform refreshes it with [`read`](Resource::read) immediately after, so
    /// returning just enough to identify it (e.g. the ID-bearing fields) is fine.
    /// Defaults to an error (import unsupported).
    async fn import(&self, _ctx: &mut Ctx, _id: String) -> Result<Self::Model, ResourceError> {
        Err(ResourceError::new("this resource does not support import"))
    }

    /// Migrate stored state written at `from_version` (an older
    /// [`SCHEMA_VERSION`](Resource::SCHEMA_VERSION)) to the current schema.
    /// `prior` is the raw stored state as a dynamic [`Value`] — it predates the
    /// current `Model`, so it is untyped (objects/lists/scalars). Defaults to an
    /// error; implement it whenever you raise `SCHEMA_VERSION`.
    async fn upgrade(
        &self,
        _ctx: &mut Ctx,
        _from_version: i64,
        _prior: Value,
    ) -> Result<Self::Model, ResourceError> {
        Err(ResourceError::new(
            "this resource does not support state upgrades",
        ))
    }

    /// Validate the resource configuration, returning any diagnostics (errors
    /// and/or warnings, optionally pointed at an attribute via [`Diag::at`]).
    /// Runs early, before planning. Attributes the user did not set — or whose
    /// values are not yet known (references to other resources) — arrive as
    /// their zero value, so guard accordingly. Defaults to no diagnostics.
    async fn validate(&self, _ctx: &mut Ctx, _config: Self::Model) -> Vec<Diag> {
        Vec::new()
    }

    /// Adjust the plan after the SDK's mechanical pass (defaults applied,
    /// `force_new` replacements and computed-unknowns marked). Return
    /// [`PlanModifications`] to force replacement by custom rule or mark an
    /// attribute unknown — each targeting an attribute [`Path`], so a nested
    /// attribute (inside a block or collection) can be reached, not just a
    /// top-level one.
    ///
    /// `prior` is the current state (`None` on create); `proposed` is the planned
    /// model. Both decode through the zero-value rule, so a computed field that
    /// planned as *unknown* reads here as its zero value — to read the
    /// known/unknown/null distinction (e.g. to drive a plan decision off whether a
    /// value is yet known), type that field [`TfValue<T>`](terraform_value::TfValue).
    /// Defaults to no modifications.
    async fn modify_plan(
        &self,
        _ctx: &mut Ctx,
        _prior: Option<Self::Model>,
        _proposed: Self::Model,
    ) -> Result<PlanModifications, ResourceError> {
        Ok(PlanModifications::default())
    }

    /// Migrate state from a *different* resource type into this one — the
    /// provider side of a `moved {}` block that crosses resource types.
    /// `source_type_name` names the source resource and `source_state` is its raw
    /// stored state as an untyped dynamic [`Value`] (the source schema may be
    /// foreign, so it is not decoded into a model). Return the migrated target
    /// model. Defaults to an error (cross-type moves unsupported); implement it to
    /// opt in, typically matching on `source_type_name`.
    async fn move_state(
        &self,
        _ctx: &mut Ctx,
        _source_type_name: String,
        _source_state: Value,
    ) -> Result<Self::Model, ResourceError> {
        Err(ResourceError::new(
            "this resource does not support moving state from another resource type",
        ))
    }

    /// Declare which attributes are *quotient types* (semantic-equality / diff
    /// suppression — roadmap 3.6). Build a [`Canon`] with
    /// [`string_quotient`](crate::string_quotient), mapping an attribute name to a
    /// canonicalizer derived from its type's own conversions, e.g.
    /// `Canon::new().with("id", string_quotient::<MyId>())`. The planner then
    /// suppresses a spurious diff/replacement when a value changes only within its
    /// equivalence class (case, ordering, normalized spelling). Defaults to none.
    ///
    /// (This is the explicit opt-in; reflection auto-harvest from the model is a
    /// follow-up gated on codec proxy-decode support — see `normalize.rs`.)
    fn semantic_equality(&self) -> Canon {
        Canon::new()
    }
}

/// Object-safe, value-oriented form of [`Resource`] that the service dispatches
/// to. Operates on the dynamic [`Value`]; the [`ResourceAdapter`] bridges to the
/// typed `Model`.
#[async_trait]
pub trait DynResource: Send + Sync {
    async fn create(&self, planned: Value) -> Result<Value, Diagnostics>;
    async fn read(&self, current: Value) -> Result<Option<Value>, Diagnostics>;
    async fn update(&self, planned: Value, prior: Value) -> Result<Value, Diagnostics>;
    async fn delete(&self, prior: Value) -> Result<(), Diagnostics>;
    async fn import(&self, id: String) -> Result<Value, Diagnostics>;
    async fn upgrade(&self, from_version: i64, prior: Value) -> Result<Value, Diagnostics>;
    async fn validate(&self, config: Value) -> Diagnostics;

    /// Adjust the mechanically-produced plan. `prior` is the prior state (null on
    /// create), `proposed` the planned value. Defaults to no modifications, so
    /// dynamic-seam implementors (e.g. the Node binding) need not implement it.
    async fn modify_plan(
        &self,
        _prior: Value,
        _proposed: Value,
    ) -> Result<PlanModifications, Diagnostics> {
        Ok(PlanModifications::default())
    }

    /// Migrate state from another resource type (a cross-type `moved {}`).
    /// `source_state` is the untyped source state; returns the target state.
    /// Defaults to an "unsupported" error, so dynamic-seam implementors (e.g. the
    /// Node binding) need not implement it.
    async fn move_state(
        &self,
        _source_type_name: String,
        _source_state: Value,
    ) -> Result<Value, Diagnostics> {
        Err(vec![Diag::error(
            "unsupported state move",
            "this resource does not support moving state from another resource type",
        )])
    }

    /// The resource's quotient-typed attributes for semantic-equality diff
    /// suppression. Defaults to none, so dynamic-seam implementors (e.g. the Node
    /// binding) need not implement it.
    fn semantic_equality(&self) -> Canon {
        Canon::new()
    }
}

/// Wraps a typed [`Resource`] as an erased [`DynResource`].
pub struct ResourceAdapter<R: Resource> {
    inner: R,
}

impl<R: Resource> ResourceAdapter<R> {
    /// Erase `resource` behind an `Arc<dyn DynResource>`.
    pub fn erased(resource: R) -> Arc<dyn DynResource> {
        Arc::new(ResourceAdapter { inner: resource })
    }
}

/// Convert a codec/decoding error into diagnostics.
pub(crate) fn codec_diag(context: &str, e: impl std::fmt::Display) -> Diagnostics {
    vec![Diag::error(format!("failed to {context}"), e.to_string())]
}

#[async_trait]
impl<R: Resource> DynResource for ResourceAdapter<R> {
    async fn create(&self, planned: Value) -> Result<Value, Diagnostics> {
        let mut ctx = current_ctx();
        let model: R::Model =
            from_value(&planned).map_err(|e| codec_diag("decode planned state", e))?;
        let result = self
            .inner
            .create(&mut ctx, model)
            .await
            .map_err(Diagnostics::from)?;
        to_value(&result).map_err(|e| codec_diag("encode new state", e))
    }

    async fn read(&self, current: Value) -> Result<Option<Value>, Diagnostics> {
        let mut ctx = current_ctx();
        let model: R::Model =
            from_value(&current).map_err(|e| codec_diag("decode current state", e))?;
        match self
            .inner
            .read(&mut ctx, model)
            .await
            .map_err(Diagnostics::from)?
        {
            Some(refreshed) => Ok(Some(
                to_value(&refreshed).map_err(|e| codec_diag("encode refreshed state", e))?,
            )),
            None => Ok(None),
        }
    }

    async fn update(&self, planned: Value, prior: Value) -> Result<Value, Diagnostics> {
        let mut ctx = current_ctx();
        let planned_model: R::Model =
            from_value(&planned).map_err(|e| codec_diag("decode planned state", e))?;
        let prior_model: R::Model =
            from_value(&prior).map_err(|e| codec_diag("decode prior state", e))?;
        let result = self
            .inner
            .update(&mut ctx, planned_model, prior_model)
            .await
            .map_err(Diagnostics::from)?;
        to_value(&result).map_err(|e| codec_diag("encode new state", e))
    }

    async fn delete(&self, prior: Value) -> Result<(), Diagnostics> {
        let mut ctx = current_ctx();
        let model: R::Model =
            from_value(&prior).map_err(|e| codec_diag("decode prior state", e))?;
        self.inner
            .delete(&mut ctx, model)
            .await
            .map_err(Diagnostics::from)
    }

    async fn import(&self, id: String) -> Result<Value, Diagnostics> {
        let mut ctx = current_ctx();
        let result = self
            .inner
            .import(&mut ctx, id)
            .await
            .map_err(Diagnostics::from)?;
        to_value(&result).map_err(|e| codec_diag("encode imported state", e))
    }

    async fn upgrade(&self, from_version: i64, prior: Value) -> Result<Value, Diagnostics> {
        let mut ctx = current_ctx();
        let result = self
            .inner
            .upgrade(&mut ctx, from_version, prior)
            .await
            .map_err(Diagnostics::from)?;
        to_value(&result).map_err(|e| codec_diag("encode upgraded state", e))
    }

    async fn validate(&self, config: Value) -> Diagnostics {
        let mut ctx = current_ctx();
        match from_value::<R::Model>(&config) {
            Ok(model) => self.inner.validate(&mut ctx, model).await,
            Err(e) => codec_diag("decode config for validation", e),
        }
    }

    async fn modify_plan(
        &self,
        prior: Value,
        proposed: Value,
    ) -> Result<PlanModifications, Diagnostics> {
        let mut ctx = current_ctx();
        let prior_model = match &prior {
            Value::Null => None,
            _ => Some(
                from_value::<R::Model>(&prior).map_err(|e| codec_diag("decode prior state", e))?,
            ),
        };
        let proposed_model: R::Model =
            from_value(&proposed).map_err(|e| codec_diag("decode proposed state", e))?;
        self.inner
            .modify_plan(&mut ctx, prior_model, proposed_model)
            .await
            .map_err(Diagnostics::from)
    }

    async fn move_state(
        &self,
        source_type_name: String,
        source_state: Value,
    ) -> Result<Value, Diagnostics> {
        let mut ctx = current_ctx();
        let result = self
            .inner
            .move_state(&mut ctx, source_type_name, source_state)
            .await
            .map_err(Diagnostics::from)?;
        to_value(&result).map_err(|e| codec_diag("encode moved state", e))
    }

    fn semantic_equality(&self) -> Canon {
        self.inner.semantic_equality()
    }
}
