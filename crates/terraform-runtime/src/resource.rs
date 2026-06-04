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
#[derive(Debug, Clone)]
pub struct ResourceError {
    /// Short, one-line summary.
    pub summary: String,
    /// Optional longer explanation.
    pub detail: String,
}

impl ResourceError {
    /// Create an error with a summary.
    pub fn new(summary: impl Into<String>) -> Self {
        ResourceError {
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
    /// A non-fatal advisory. Not yet producible by handlers; reserved.
    #[allow(dead_code)]
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
}

impl Diag {
    /// An error diagnostic.
    pub fn error(summary: impl Into<String>, detail: impl Into<String>) -> Self {
        Diag {
            severity: Severity::Error,
            summary: summary.into(),
            detail: detail.into(),
        }
    }
}

impl From<ResourceError> for Diag {
    fn from(e: ResourceError) -> Self {
        Diag {
            severity: Severity::Error,
            summary: e.summary,
            detail: e.detail,
        }
    }
}

/// A collection of diagnostics returned from an erased resource operation.
pub type Diagnostics = Vec<Diag>;

/// A managed resource type.
///
/// Implement this over a `Model` struct that reflects (via `#[derive(Facet)]`)
/// the resource's schema. `create` is required; `read` defaults to returning the
/// state unchanged, `update` to an error, and `delete` to a no-op.
#[async_trait]
pub trait Resource: Send + Sync + 'static {
    /// The Rust type modeling this resource's schema (config + computed state).
    type Model: Facet<'static> + Send + Sync;

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
fn codec_diag(context: &str, e: impl std::fmt::Display) -> Diagnostics {
    vec![Diag::error(format!("failed to {context}"), e.to_string())]
}

#[async_trait]
impl<R: Resource> DynResource for ResourceAdapter<R> {
    async fn create(&self, planned: Value) -> Result<Value, Diagnostics> {
        let model: R::Model =
            from_value(&planned).map_err(|e| codec_diag("decode planned state", e))?;
        let result = self
            .inner
            .create(model)
            .await
            .map_err(Diag::from)
            .map_err(|d| vec![d])?;
        to_value(&result).map_err(|e| codec_diag("encode new state", e))
    }

    async fn read(&self, current: Value) -> Result<Option<Value>, Diagnostics> {
        let model: R::Model =
            from_value(&current).map_err(|e| codec_diag("decode current state", e))?;
        match self
            .inner
            .read(model)
            .await
            .map_err(Diag::from)
            .map_err(|d| vec![d])?
        {
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
            .map_err(Diag::from)
            .map_err(|d| vec![d])?;
        to_value(&result).map_err(|e| codec_diag("encode new state", e))
    }

    async fn delete(&self, prior: Value) -> Result<(), Diagnostics> {
        let model: R::Model =
            from_value(&prior).map_err(|e| codec_diag("decode prior state", e))?;
        self.inner
            .delete(model)
            .await
            .map_err(Diag::from)
            .map_err(|d| vec![d])
    }
}
