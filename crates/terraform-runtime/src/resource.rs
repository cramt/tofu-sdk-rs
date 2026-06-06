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

/// Adjustments a resource's [`Resource::modify_plan`] makes to the
/// mechanically-produced plan: extra attributes to force replacement, and
/// computed attributes to mark unknown by custom rule. Both name **top-level**
/// attributes (nested-block paths are a future refinement).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanModifications {
    /// Top-level attribute names whose planned change should force replacement,
    /// in addition to the `force_new` attributes handled mechanically.
    pub require_replace: Vec<String>,
    /// Top-level attribute names to mark unknown in the planned state (a value
    /// the provider will compute by rule during apply).
    pub unknown: Vec<String>,
}

impl PlanModifications {
    /// No modifications (the default).
    pub fn new() -> Self {
        Self::default()
    }

    /// Force replacement when the named top-level attribute changes.
    pub fn require_replace(mut self, name: impl Into<String>) -> Self {
        self.require_replace.push(name.into());
        self
    }

    /// Mark the named top-level attribute unknown in the plan.
    pub fn unknown(mut self, name: impl Into<String>) -> Self {
        self.unknown.push(name.into());
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
    /// computed attributes filled in.
    async fn create(&self, planned: Self::Model) -> Result<Self::Model, ResourceError>;

    /// Refresh `current` from the real system. Return `None` if it no longer
    /// exists (Terraform will plan to recreate it). Defaults to returning
    /// `current` unchanged.
    async fn read(&self, current: Self::Model) -> Result<Option<Self::Model>, ResourceError> {
        Ok(Some(current))
    }

    /// Update an existing resource to its planned state. Defaults to an error.
    async fn update(
        &self,
        _planned: Self::Model,
        _prior: Self::Model,
    ) -> Result<Self::Model, ResourceError> {
        Err(ResourceError::new(
            "this resource does not support in-place update",
        ))
    }

    /// Delete the resource. Defaults to a no-op.
    async fn delete(&self, _prior: Self::Model) -> Result<(), ResourceError> {
        Ok(())
    }

    /// Import an existing resource by its ID, returning the imported state.
    /// Terraform refreshes it with [`read`](Resource::read) immediately after, so
    /// returning just enough to identify it (e.g. the ID-bearing fields) is fine.
    /// Defaults to an error (import unsupported).
    async fn import(&self, _id: String) -> Result<Self::Model, ResourceError> {
        Err(ResourceError::new("this resource does not support import"))
    }

    /// Migrate stored state written at `from_version` (an older
    /// [`SCHEMA_VERSION`](Resource::SCHEMA_VERSION)) to the current schema.
    /// `prior` is the raw stored state as a dynamic [`Value`] — it predates the
    /// current `Model`, so it is untyped (objects/lists/scalars). Defaults to an
    /// error; implement it whenever you raise `SCHEMA_VERSION`.
    async fn upgrade(
        &self,
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
    async fn validate(&self, _config: Self::Model) -> Vec<Diag> {
        Vec::new()
    }

    /// Adjust the plan after the SDK's mechanical pass (defaults applied,
    /// `force_new` replacements and computed-unknowns marked). Return
    /// [`PlanModifications`] to force replacement by custom rule or mark a
    /// computed attribute unknown.
    ///
    /// `prior` is the current state (`None` on create); `proposed` is the planned
    /// model. Note both decode through the zero-value rule, so a computed field
    /// that planned as *unknown* reads here as its zero value — compare the
    /// config-driven fields, not computed ones. Defaults to no modifications.
    async fn modify_plan(
        &self,
        _prior: Option<Self::Model>,
        _proposed: Self::Model,
    ) -> Result<PlanModifications, ResourceError> {
        Ok(PlanModifications::default())
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
        let model: R::Model =
            from_value(&planned).map_err(|e| codec_diag("decode planned state", e))?;
        let result = self.inner.create(model).await.map_err(Diagnostics::from)?;
        to_value(&result).map_err(|e| codec_diag("encode new state", e))
    }

    async fn read(&self, current: Value) -> Result<Option<Value>, Diagnostics> {
        let model: R::Model =
            from_value(&current).map_err(|e| codec_diag("decode current state", e))?;
        match self.inner.read(model).await.map_err(Diagnostics::from)? {
            Some(refreshed) => Ok(Some(
                to_value(&refreshed).map_err(|e| codec_diag("encode refreshed state", e))?,
            )),
            None => Ok(None),
        }
    }

    async fn update(&self, planned: Value, prior: Value) -> Result<Value, Diagnostics> {
        let planned_model: R::Model =
            from_value(&planned).map_err(|e| codec_diag("decode planned state", e))?;
        let prior_model: R::Model =
            from_value(&prior).map_err(|e| codec_diag("decode prior state", e))?;
        let result = self
            .inner
            .update(planned_model, prior_model)
            .await
            .map_err(Diagnostics::from)?;
        to_value(&result).map_err(|e| codec_diag("encode new state", e))
    }

    async fn delete(&self, prior: Value) -> Result<(), Diagnostics> {
        let model: R::Model =
            from_value(&prior).map_err(|e| codec_diag("decode prior state", e))?;
        self.inner.delete(model).await.map_err(Diagnostics::from)
    }

    async fn import(&self, id: String) -> Result<Value, Diagnostics> {
        let result = self.inner.import(id).await.map_err(Diagnostics::from)?;
        to_value(&result).map_err(|e| codec_diag("encode imported state", e))
    }

    async fn upgrade(&self, from_version: i64, prior: Value) -> Result<Value, Diagnostics> {
        let result = self
            .inner
            .upgrade(from_version, prior)
            .await
            .map_err(Diagnostics::from)?;
        to_value(&result).map_err(|e| codec_diag("encode upgraded state", e))
    }

    async fn validate(&self, config: Value) -> Diagnostics {
        match from_value::<R::Model>(&config) {
            Ok(model) => self.inner.validate(model).await,
            Err(e) => codec_diag("decode config for validation", e),
        }
    }

    async fn modify_plan(
        &self,
        prior: Value,
        proposed: Value,
    ) -> Result<PlanModifications, Diagnostics> {
        let prior_model = match &prior {
            Value::Null => None,
            _ => Some(
                from_value::<R::Model>(&prior).map_err(|e| codec_diag("decode prior state", e))?,
            ),
        };
        let proposed_model: R::Model =
            from_value(&proposed).map_err(|e| codec_diag("decode proposed state", e))?;
        self.inner
            .modify_plan(prior_model, proposed_model)
            .await
            .map_err(Diagnostics::from)
    }
}
