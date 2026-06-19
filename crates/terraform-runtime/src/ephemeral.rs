//! The author-facing [`Ephemeral`] trait and its internal type erasure.
//!
//! An ephemeral resource produces a value for the duration of a single
//! Terraform operation and is **never written to state**. Its lifecycle is
//! `Open` → (optional `Renew`) → `Close`, not the managed-resource CRUD:
//!
//! - [`Ephemeral::open`] runs during *both* plan and apply (the result may be
//!   needed to configure a provider at plan time). It returns the result value
//!   and may stash a handle into [`Ctx::set_private`] and request renewal via
//!   [`Ctx::set_renew_at`] / [`Ctx::renew_after`].
//! - [`Ephemeral::renew`] keeps a lease alive; defaulted to a no-op (most
//!   ephemeral resources never expire).
//! - [`Ephemeral::close`] tears the resource down; defaulted to a no-op (a pure
//!   reader has nothing to release).
//!
//! **`Renew`/`Close` receive only the private bytes, never the config or the
//! result** — the protocol hands them `type_name` + `private` and nothing else.
//! So whatever those handlers need (a lease ID, a created object's ID) must be
//! serialized into [`Ctx::set_private`] during `open`.
//!
//! Authors implement this over a plain Rust `Model`; the runtime wraps each
//! handler in an [`EphemeralAdapter`] that bridges the dynamic [`Value`] and the
//! typed `Model`. The erased [`DynEphemeral`] is what the gRPC service dispatches
//! to for `Open`/`Renew`/`Close`/`ValidateEphemeralResourceConfig`.

use std::sync::Arc;

use async_trait::async_trait;
use facet::Facet;
use terraform_codec::{from_value, to_value};
use terraform_value::Value;

use crate::ctx::{current_ctx, Ctx};
use crate::resource::{codec_diag, Diag, Diagnostics, Resource, ResourceError, Severity};

/// An error returned by an ephemeral resource operation, surfaced to Terraform
/// as an error diagnostic.
#[derive(Debug, Clone)]
pub struct EphemeralError {
    /// Short, one-line summary.
    pub summary: String,
    /// Optional longer explanation.
    pub detail: String,
}

impl EphemeralError {
    /// Create an error with a summary.
    pub fn new(summary: impl Into<String>) -> Self {
        EphemeralError {
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

impl From<&str> for EphemeralError {
    fn from(s: &str) -> Self {
        EphemeralError::new(s)
    }
}

impl From<String> for EphemeralError {
    fn from(s: String) -> Self {
        EphemeralError::new(s)
    }
}

impl From<ResourceError> for EphemeralError {
    /// Used by [`EphemeralFromResource`]: a wrapped resource's create/delete
    /// error becomes an ephemeral error (attribute paths and attached warnings
    /// are dropped — ephemeral operations carry no per-attribute plan).
    fn from(e: ResourceError) -> Self {
        EphemeralError {
            summary: e.summary,
            detail: e.detail,
        }
    }
}

impl From<EphemeralError> for Diag {
    fn from(e: EphemeralError) -> Self {
        Diag {
            severity: Severity::Error,
            summary: e.summary,
            detail: e.detail,
            attribute: Vec::new(),
        }
    }
}

/// An ephemeral resource type.
///
/// Implement this over a `Model` struct that reflects (via `#[derive(Facet)]`)
/// the resource's schema: plain fields are config inputs, `#[facet(terraform::
/// computed)]` fields are the result `open` fills. Only `open` is required.
#[async_trait]
pub trait Ephemeral: Send + Sync + 'static {
    /// The Rust type modeling this ephemeral resource's schema (config inputs +
    /// computed result).
    type Model: Facet<'static> + Send + Sync;

    /// Open the ephemeral resource from its configuration and return the result
    /// with computed attributes filled in. Runs during *both* plan and apply, so
    /// it must be safe to call at plan time (and safe to abandon if the operation
    /// is interrupted). Use `ctx` to stash a handle for `renew`/`close`
    /// ([`Ctx::set_private`]), request renewal ([`Ctx::renew_after`]), or emit
    /// success warnings.
    async fn open(&self, ctx: &mut Ctx, config: Self::Model)
        -> Result<Self::Model, EphemeralError>;

    /// Renew a lease before its [`Ctx::set_renew_at`] deadline. Read the handle
    /// from [`Ctx::private`]; optionally push the deadline forward again. Defaults
    /// to a no-op (the resource does not expire).
    async fn renew(&self, _ctx: &mut Ctx) -> Result<(), EphemeralError> {
        Ok(())
    }

    /// Tear the ephemeral resource down at the end of the operation. Read the
    /// handle from [`Ctx::private`]. Defaults to a no-op (a pure reader holds
    /// nothing to release).
    async fn close(&self, _ctx: &mut Ctx) -> Result<(), EphemeralError> {
        Ok(())
    }

    /// Validate the configuration, returning any diagnostics. Runs before `open`;
    /// unset/unknown attributes arrive as their zero value. Defaults to none.
    async fn validate(&self, _ctx: &mut Ctx, _config: Self::Model) -> Vec<Diag> {
        Vec::new()
    }
}

/// Object-safe, value-oriented form of [`Ephemeral`] that the service dispatches
/// to. Operates on the dynamic [`Value`]; the [`EphemeralAdapter`] bridges to the
/// typed `Model`. `renew`/`close` take no value — they reach the stashed handle
/// through the ambient [`Ctx`]'s private state.
#[async_trait]
pub trait DynEphemeral: Send + Sync {
    async fn open(&self, config: Value) -> Result<Value, Diagnostics>;
    async fn renew(&self) -> Result<(), Diagnostics>;
    async fn close(&self) -> Result<(), Diagnostics>;
    async fn validate(&self, config: Value) -> Diagnostics;
}

/// Wraps a typed [`Ephemeral`] as an erased [`DynEphemeral`].
pub struct EphemeralAdapter<E: Ephemeral> {
    inner: E,
}

impl<E: Ephemeral> EphemeralAdapter<E> {
    /// Erase `ephemeral` behind an `Arc<dyn DynEphemeral>`.
    pub fn erased(ephemeral: E) -> Arc<dyn DynEphemeral> {
        Arc::new(EphemeralAdapter { inner: ephemeral })
    }
}

#[async_trait]
impl<E: Ephemeral> DynEphemeral for EphemeralAdapter<E> {
    async fn open(&self, config: Value) -> Result<Value, Diagnostics> {
        let mut ctx = current_ctx();
        let model: E::Model =
            from_value(&config).map_err(|e| codec_diag("decode ephemeral config", e))?;
        let result = self
            .inner
            .open(&mut ctx, model)
            .await
            .map_err(Diag::from)
            .map_err(|d| vec![d])?;
        to_value(&result).map_err(|e| codec_diag("encode ephemeral result", e))
    }

    async fn renew(&self) -> Result<(), Diagnostics> {
        let mut ctx = current_ctx();
        self.inner
            .renew(&mut ctx)
            .await
            .map_err(Diag::from)
            .map_err(|d| vec![d])
    }

    async fn close(&self) -> Result<(), Diagnostics> {
        let mut ctx = current_ctx();
        self.inner
            .close(&mut ctx)
            .await
            .map_err(Diag::from)
            .map_err(|d| vec![d])
    }

    async fn validate(&self, config: Value) -> Diagnostics {
        let mut ctx = current_ctx();
        match from_value::<E::Model>(&config) {
            Ok(model) => self.inner.validate(&mut ctx, model).await,
            Err(e) => codec_diag("decode ephemeral config for validation", e),
        }
    }
}

/// Expose an existing managed [`Resource`] as an ephemeral resource by mapping
/// `Open` → `create` and `Close` → `delete`.
///
/// This is the opt-in auto-derive: for a **cheap, reversible** resource (a
/// firewall rule, a temporary grant) you can reuse the CRUD you already wrote
/// instead of writing a separate [`Ephemeral`] impl. Register it like any other
/// ephemeral handler:
///
/// ```ignore
/// builder.ephemeral(EphemeralFromResource(MyFirewallRule));
/// // or, meta-backed:
/// builder.ephemeral_with(|client| EphemeralFromResource(MyFirewallRule { client }));
/// ```
///
/// **Caveats — use only where create-then-delete-each-run is safe:**
/// - There is **no `Renew`** (managed resources have no such concept), so it is
///   unsuitable for leases that expire mid-operation.
/// - If the operation is interrupted between `open` and `close`, the created
///   object **leaks** — there is no state row to clean it up later. Self-expiring
///   resources (those with a TTL) avoid this; arbitrary infra does not.
///
/// `open` stashes the created model into private state (as JSON) so `close` can
/// reconstruct it and call `delete` — the protocol gives `close` nothing else.
pub struct EphemeralFromResource<R>(pub R);

#[async_trait]
impl<R: Resource> Ephemeral for EphemeralFromResource<R> {
    type Model = R::Model;

    async fn open(
        &self,
        ctx: &mut Ctx,
        config: Self::Model,
    ) -> Result<Self::Model, EphemeralError> {
        let created = self.0.create(ctx, config).await?;
        let json = facet_json::to_string(&created).map_err(|e| {
            EphemeralError::new("failed to record ephemeral handle").with_detail(e.to_string())
        })?;
        ctx.set_private(json.into_bytes());
        Ok(created)
    }

    async fn close(&self, ctx: &mut Ctx) -> Result<(), EphemeralError> {
        let private = ctx.private().to_vec();
        if private.is_empty() {
            // Nothing was opened (or already closed); nothing to delete.
            return Ok(());
        }
        let prior: R::Model = facet_json::from_slice(&private).map_err(|e| {
            EphemeralError::new("failed to read ephemeral handle for close")
                .with_detail(e.to_string())
        })?;
        self.0.delete(ctx, prior).await?;
        Ok(())
    }

    async fn validate(&self, ctx: &mut Ctx, config: Self::Model) -> Vec<Diag> {
        // Reuse the wrapped resource's validation.
        self.0.validate(ctx, config).await
    }
}
