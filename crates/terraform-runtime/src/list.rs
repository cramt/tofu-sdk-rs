//! The author-facing [`ListResource`] trait and its internal type erasure.
//!
//! A **list resource** enumerates existing instances of the managed resource of
//! the **same type name**: given a query/filter config, it yields each matching
//! instance as a resource *identity* (and, when the host asks, the full resource
//! object). It backs Terraform's `list {}` blocks and the import-discovery flow.
//!
//! The coupling to the managed resource is expressed purely through the **shared
//! `Model` type**: a list resource names the same `Model` as the managed resource,
//! so the type name ([`resource_name`](terraform_reflect::resource_name)), the
//! identity schema, and the full object type all line up by construction — there
//! is no stringly-typed association to keep in sync. The author additionally
//! supplies a `Config` type for the list block's query inputs.
//!
//! Authors implement [`ListResource::list`] returning a `Vec` of [`ListItem`]s;
//! the runtime wraps it in a [`ListResourceAdapter`] that bridges the dynamic
//! [`Value`] and the typed model. The erased [`DynListResource`] is what the gRPC
//! service dispatches `ListResource` to.

use std::sync::Arc;

use async_trait::async_trait;
use facet::Facet;
use terraform_codec::{from_value, to_value};
use terraform_value::Value;

use crate::ctx::{current_ctx, Ctx};
use crate::resource::{codec_diag, Diag, Diagnostics, Severity};

/// An error returned by a list operation, surfaced to Terraform as an error
/// diagnostic on the result stream.
#[derive(Debug, Clone)]
pub struct ListError {
    /// Short, one-line summary.
    pub summary: String,
    /// Optional longer explanation.
    pub detail: String,
}

impl ListError {
    /// Create an error with a summary.
    pub fn new(summary: impl Into<String>) -> Self {
        ListError {
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

impl From<&str> for ListError {
    fn from(s: &str) -> Self {
        ListError::new(s)
    }
}

impl From<String> for ListError {
    fn from(s: String) -> Self {
        ListError::new(s)
    }
}

impl From<ListError> for Diag {
    fn from(e: ListError) -> Self {
        Diag {
            severity: Severity::Error,
            summary: e.summary,
            detail: e.detail,
            attribute: Vec::new(),
        }
    }
}

/// One result of a list operation: a managed-resource `Model` plus a human-facing
/// display name. The runtime projects the model into the result's identity and,
/// when the host requests it, encodes it as the full `resource_object`.
pub struct ListItem<M> {
    /// A human-readable label for the instance (shown in UI / plan output).
    pub display_name: String,
    /// The listed instance, as the managed resource's model.
    pub resource: M,
}

impl<M> ListItem<M> {
    /// Build a result from a display name and the managed-resource model.
    pub fn new(display_name: impl Into<String>, resource: M) -> Self {
        ListItem {
            display_name: display_name.into(),
            resource,
        }
    }
}

/// A list resource type: a queryable enumeration of existing instances of the
/// managed resource of the same name.
///
/// Implement this over the managed resource's `Model` (which must declare an
/// identity via `#[facet(terraform::identity)]`) and a `Config` type for the
/// list block's query inputs. Only [`list`](ListResource::list) is required.
#[async_trait]
pub trait ListResource: Send + Sync + 'static {
    /// The managed resource's model — the same type used to register the resource,
    /// so identity and object schema are shared by construction. Must declare at
    /// least one `#[facet(terraform::identity)]` field.
    type Model: Facet<'static> + Send + Sync;

    /// The query/filter configuration for the `list {}` block.
    type Config: Facet<'static> + Send + Sync;

    /// Enumerate the existing instances matching `config`. Each [`ListItem`]
    /// carries the managed-resource model (projected into the result's identity /
    /// object) and a display name. Use `ctx` to emit success warnings or observe
    /// cancellation.
    async fn list(
        &self,
        ctx: &mut Ctx,
        config: Self::Config,
    ) -> Result<Vec<ListItem<Self::Model>>, ListError>;

    /// Validate the list configuration, returning any diagnostics. Runs before
    /// `list`; unset/unknown attributes arrive as their zero value. Defaults to
    /// none.
    async fn validate(&self, _ctx: &mut Ctx, _config: Self::Config) -> Vec<Diag> {
        Vec::new()
    }
}

/// One erased list result: a display name plus the managed-resource model encoded
/// as a dynamic [`Value`]. The service projects identity and (optionally) the full
/// object from this `Value`.
pub struct DynListItem {
    /// Human-readable label for the instance.
    pub display_name: String,
    /// The managed-resource model as a dynamic object value.
    pub object: Value,
}

/// Object-safe, value-oriented form of [`ListResource`] that the service
/// dispatches to. Operates on the dynamic [`Value`]; the [`ListResourceAdapter`]
/// bridges to the typed model.
#[async_trait]
pub trait DynListResource: Send + Sync {
    async fn list(&self, config: Value) -> Result<Vec<DynListItem>, Diagnostics>;
    async fn validate(&self, config: Value) -> Diagnostics;
}

/// Wraps a typed [`ListResource`] as an erased [`DynListResource`].
pub struct ListResourceAdapter<L: ListResource> {
    inner: L,
}

impl<L: ListResource> ListResourceAdapter<L> {
    /// Erase `list` behind an `Arc<dyn DynListResource>`.
    pub fn erased(list: L) -> Arc<dyn DynListResource> {
        Arc::new(ListResourceAdapter { inner: list })
    }
}

#[async_trait]
impl<L: ListResource> DynListResource for ListResourceAdapter<L> {
    async fn list(&self, config: Value) -> Result<Vec<DynListItem>, Diagnostics> {
        let mut ctx = current_ctx();
        let cfg: L::Config =
            from_value(&config).map_err(|e| codec_diag("decode list config", e))?;
        let items = self
            .inner
            .list(&mut ctx, cfg)
            .await
            .map_err(Diag::from)
            .map_err(|d| vec![d])?;
        items
            .into_iter()
            .map(|item| {
                to_value(&item.resource)
                    .map(|object| DynListItem {
                        display_name: item.display_name,
                        object,
                    })
                    .map_err(|e| codec_diag("encode list result", e))
            })
            .collect()
    }

    async fn validate(&self, config: Value) -> Diagnostics {
        let mut ctx = current_ctx();
        match from_value::<L::Config>(&config) {
            Ok(cfg) => self.inner.validate(&mut ctx, cfg).await,
            Err(e) => codec_diag("decode list config for validation", e),
        }
    }
}
