//! The author-facing [`Action`] trait and its internal type erasure.
//!
//! A **provider-defined action** is an imperative operation a configuration can
//! trigger (an `action "<type>" "<label>" {}` block invoked during apply) — the
//! provider equivalent of a one-shot side effect like "send a notification" or
//! "invoke a Lambda". It is a separate protocol primitive from resources: it has
//! no state, no plan diff, no lifecycle — only a config block and three calls.
//!
//! Authoring is a single trait over a `Config` model (the action's inputs,
//! reflected into the published schema block):
//!
//! - [`validate`](Action::validate) — check the config (`ValidateActionConfig`).
//! - [`plan`](Action::plan) — a dry run during planning (`PlanAction`); raise a
//!   diagnostic to fail the plan. Defaults to a no-op.
//! - [`invoke`](Action::invoke) — perform the side effect (`InvokeAction`).
//!   Progress messages stream to the host via [`Ctx::progress`](crate::Ctx::progress).
//!
//! The type name is supplied at registration (like a function / state store),
//! since an action's name is not tied to a model identity. The typed trait erases
//! to [`DynAction`], the value-oriented seam the gRPC service dispatches to (and
//! which a non-Rust frontend can implement directly via
//! [`ProviderBuilder::dyn_action`](crate::ProviderBuilder::dyn_action)).

use std::sync::Arc;

use async_trait::async_trait;
use facet::Facet;
use terraform_codec::from_value;
use terraform_value::Value;

use crate::ctx::{current_ctx, Ctx};
use crate::resource::{codec_diag, Diag, Diagnostics, Severity};

/// An error returned by an action's `plan`/`invoke`, surfaced to Terraform as an
/// error diagnostic.
#[derive(Debug, Clone)]
pub struct ActionError {
    /// Short, one-line summary.
    pub summary: String,
    /// Optional longer explanation.
    pub detail: String,
}

impl ActionError {
    /// Create an error with a summary.
    pub fn new(summary: impl Into<String>) -> Self {
        ActionError {
            summary: summary.into(),
            detail: String::new(),
        }
    }

    /// Attach a longer detail message.
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }
}

impl From<&str> for ActionError {
    fn from(s: &str) -> Self {
        ActionError::new(s)
    }
}

impl From<String> for ActionError {
    fn from(s: String) -> Self {
        ActionError::new(s)
    }
}

impl From<ActionError> for Diag {
    fn from(e: ActionError) -> Self {
        Diag {
            severity: Severity::Error,
            summary: e.summary,
            detail: e.detail,
            attribute: Vec::new(),
        }
    }
}

/// A provider-defined action: an imperative, stateless operation triggered from
/// configuration.
///
/// Implement this over a `Config` struct (the action's inputs, reflected via
/// `#[derive(Facet)]` into the published schema). Only [`invoke`](Action::invoke)
/// is required.
#[async_trait]
pub trait Action: Send + Sync + 'static {
    /// The Rust type modeling the action's configuration block.
    type Config: Facet<'static> + Send + Sync;

    /// Validate the configuration, returning any diagnostics. Runs in
    /// `ValidateActionConfig`; unset/unknown attributes arrive as their zero
    /// value. Defaults to none.
    async fn validate(&self, _ctx: &mut Ctx, _config: Self::Config) -> Vec<Diag> {
        Vec::new()
    }

    /// A dry run during planning (`PlanAction`): inspect the config and return an
    /// [`ActionError`] to fail the plan, or `Ok(())` to allow it. Defaults to a
    /// no-op (the action always plans cleanly).
    async fn plan(&self, _ctx: &mut Ctx, _config: Self::Config) -> Result<(), ActionError> {
        Ok(())
    }

    /// Perform the action's side effect (`InvokeAction`). Emit progress with
    /// [`ctx.progress(...)`](crate::Ctx::progress); return an [`ActionError`] to
    /// fail the invocation.
    async fn invoke(&self, ctx: &mut Ctx, config: Self::Config) -> Result<(), ActionError>;
}

/// Object-safe, value-oriented form of [`Action`] that the service dispatches to.
/// The [`ActionAdapter`] bridges to the typed `Config`.
#[async_trait]
pub trait DynAction: Send + Sync {
    /// Validate the decoded config block.
    async fn validate(&self, config: Value) -> Diagnostics;
    /// Dry-run the action against the decoded config block.
    async fn plan(&self, config: Value) -> Result<(), Diagnostics>;
    /// Invoke the action against the decoded config block.
    async fn invoke(&self, config: Value) -> Result<(), Diagnostics>;
}

/// Wraps a typed [`Action`] as an erased [`DynAction`].
pub struct ActionAdapter<A: Action> {
    inner: A,
}

impl<A: Action> ActionAdapter<A> {
    /// Erase `action` behind an `Arc<dyn DynAction>`.
    pub fn erased(action: A) -> Arc<dyn DynAction> {
        Arc::new(ActionAdapter { inner: action })
    }
}

#[async_trait]
impl<A: Action> DynAction for ActionAdapter<A> {
    async fn validate(&self, config: Value) -> Diagnostics {
        let mut ctx = current_ctx();
        match from_value::<A::Config>(&config) {
            Ok(cfg) => self.inner.validate(&mut ctx, cfg).await,
            Err(e) => codec_diag("decode action config for validation", e),
        }
    }

    async fn plan(&self, config: Value) -> Result<(), Diagnostics> {
        let mut ctx = current_ctx();
        let cfg: A::Config =
            from_value(&config).map_err(|e| codec_diag("decode action config", e))?;
        self.inner
            .plan(&mut ctx, cfg)
            .await
            .map_err(Diag::from)
            .map_err(|d| vec![d])
    }

    async fn invoke(&self, config: Value) -> Result<(), Diagnostics> {
        let mut ctx = current_ctx();
        let cfg: A::Config =
            from_value(&config).map_err(|e| codec_diag("decode action config", e))?;
        self.inner
            .invoke(&mut ctx, cfg)
            .await
            .map_err(Diag::from)
            .map_err(|d| vec![d])
    }
}
