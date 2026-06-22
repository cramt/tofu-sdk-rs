//! The author-facing [`StateStore`] / [`StateBackend`] traits and type erasure.
//!
//! A **state store** is a Terraform *backend* implemented by a provider: it
//! validates and accepts a configuration block, then reads and writes the raw
//! state bytes for each named state ("workspace") and manages locks. It is a
//! separate protocol primitive from resources/data sources — its operations are
//! byte- and lock-oriented, not model-oriented.
//!
//! Authoring is a **two-trait split** that mirrors provider configuration:
//!
//! - [`StateStore`] is the registered handler. It reflects a `Config` model into
//!   the published schema block and, on `ConfigureStateStore`, turns the decoded
//!   config into a connected [`StateBackend`] (e.g. an S3 client bound to a
//!   bucket). This is the state-store analog of `configure` → meta.
//! - [`StateBackend`] is the configured connection. Its methods are the byte/lock
//!   operations, each keyed by a `state_id`. The protocol passes only the
//!   `state_id` (and, for writes, the bytes) — never the config again — so the
//!   backend must hold whatever it needs from `configure`.
//!
//! The runtime streams `ReadStateBytes`/`WriteStateBytes` in chunks, but the
//! handler sees whole byte vectors: reads return the full state, writes receive
//! the reassembled bytes. State files are bounded (a single workspace's state), so
//! materializing is fine — the same trade-off list resources make.
//!
//! Both traits erase to [`DynStateStore`]/[`DynStateBackend`], the value-oriented
//! seam the gRPC service dispatches to (and which a non-Rust frontend could
//! implement directly via [`ProviderBuilder::dyn_state_store`]).
//!
//! [`ProviderBuilder::dyn_state_store`]: crate::ProviderBuilder::dyn_state_store

use std::sync::Arc;

use async_trait::async_trait;
use facet::Facet;
use terraform_codec::from_value;
use terraform_value::Value;

use crate::resource::{codec_diag, Diag, Diagnostics, Severity};

/// An error returned by a state store operation, surfaced to Terraform as an
/// error diagnostic.
#[derive(Debug, Clone)]
pub struct StateStoreError {
    /// Short, one-line summary.
    pub summary: String,
    /// Optional longer explanation.
    pub detail: String,
}

impl StateStoreError {
    /// Create an error with a summary.
    pub fn new(summary: impl Into<String>) -> Self {
        StateStoreError {
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

impl From<&str> for StateStoreError {
    fn from(s: &str) -> Self {
        StateStoreError::new(s)
    }
}

impl From<String> for StateStoreError {
    fn from(s: String) -> Self {
        StateStoreError::new(s)
    }
}

impl From<StateStoreError> for Diag {
    fn from(e: StateStoreError) -> Self {
        Diag {
            severity: Severity::Error,
            summary: e.summary,
            detail: e.detail,
            attribute: Vec::new(),
        }
    }
}

/// A provider-defined state store: a configurable Terraform backend.
///
/// Implement this over a `Config` struct (the backend configuration, reflected
/// via `#[derive(Facet)]` into the published schema) and a `Backend` type holding
/// the connected state. [`configure`](StateStore::configure) builds the backend
/// once, when Terraform calls `ConfigureStateStore`.
#[async_trait]
pub trait StateStore: Send + Sync + 'static {
    /// The Rust type modeling the backend's configuration block.
    type Config: Facet<'static> + Send + Sync;

    /// The connected backend that performs the byte/lock operations.
    type Backend: StateBackend;

    /// Connect the backend from its decoded configuration. Runs on
    /// `ConfigureStateStore` (after the provider itself is configured). Return a
    /// [`StateStoreError`] to reject bad configuration / an unreachable backend.
    async fn configure(&self, config: Self::Config) -> Result<Self::Backend, StateStoreError>;

    /// Validate the configuration, returning any diagnostics. Runs in
    /// `ValidateStateStoreConfig`, before `configure`; unset/unknown attributes
    /// arrive as their zero value. Defaults to none.
    async fn validate(&self, _config: Self::Config) -> Vec<Diag> {
        Vec::new()
    }
}

/// The connected backend of a [`StateStore`]: the byte- and lock-level operations
/// over named states ("workspaces"), each keyed by a `state_id`.
///
/// The protocol hands these methods only the `state_id` (and, for `write_state`,
/// the bytes), never the configuration — so the backend must hold its connection
/// and settings from [`StateStore::configure`].
#[async_trait]
pub trait StateBackend: Send + Sync + 'static {
    /// Read the full state bytes for `state_id`. An empty `Vec` denotes an absent
    /// or empty state (Terraform treats it as a fresh state).
    async fn read_state(&self, state_id: String) -> Result<Vec<u8>, StateStoreError>;

    /// Write the full state bytes for `state_id`, replacing any prior state.
    async fn write_state(&self, state_id: String, data: Vec<u8>) -> Result<(), StateStoreError>;

    /// Acquire a lock on `state_id` for `operation` (e.g. `"plan"`, `"apply"`),
    /// returning a lock identifier the matching [`unlock`](StateBackend::unlock)
    /// will carry. Return an error if the state is already locked.
    async fn lock(&self, state_id: String, operation: String) -> Result<String, StateStoreError>;

    /// Release the lock `lock_id` previously returned for `state_id`.
    async fn unlock(&self, state_id: String, lock_id: String) -> Result<(), StateStoreError>;

    /// Enumerate the identifiers of every state ("workspace") this backend holds.
    async fn states(&self) -> Result<Vec<String>, StateStoreError>;

    /// Delete the state `state_id` entirely.
    async fn delete_state(&self, state_id: String) -> Result<(), StateStoreError>;
}

/// Object-safe, value-oriented form of [`StateStore`] that the service dispatches
/// to. The [`StateStoreAdapter`] bridges to the typed `Config`/`Backend`.
#[async_trait]
pub trait DynStateStore: Send + Sync {
    /// Validate the decoded config block.
    async fn validate(&self, config: Value) -> Diagnostics;
    /// Build the erased backend from the decoded config block.
    async fn configure(&self, config: Value) -> Result<Arc<dyn DynStateBackend>, Diagnostics>;
}

/// Object-safe form of [`StateBackend`]: the byte/lock operations on the erased
/// seam. The runtime stores one of these per configured state store.
#[async_trait]
pub trait DynStateBackend: Send + Sync {
    /// Read the full state bytes for `state_id` (empty = absent/empty).
    async fn read_state(&self, state_id: String) -> Result<Vec<u8>, Diagnostics>;
    /// Write the full state bytes for `state_id`.
    async fn write_state(&self, state_id: String, data: Vec<u8>) -> Result<(), Diagnostics>;
    /// Acquire a lock, returning its identifier.
    async fn lock(&self, state_id: String, operation: String) -> Result<String, Diagnostics>;
    /// Release a lock by identifier.
    async fn unlock(&self, state_id: String, lock_id: String) -> Result<(), Diagnostics>;
    /// Enumerate the held state identifiers.
    async fn states(&self) -> Result<Vec<String>, Diagnostics>;
    /// Delete a state entirely.
    async fn delete_state(&self, state_id: String) -> Result<(), Diagnostics>;
}

/// Wraps a typed [`StateStore`] as an erased [`DynStateStore`].
pub struct StateStoreAdapter<S: StateStore> {
    inner: S,
}

impl<S: StateStore> StateStoreAdapter<S> {
    /// Erase `store` behind an `Arc<dyn DynStateStore>`.
    pub fn erased(store: S) -> Arc<dyn DynStateStore> {
        Arc::new(StateStoreAdapter { inner: store })
    }
}

#[async_trait]
impl<S: StateStore> DynStateStore for StateStoreAdapter<S> {
    async fn validate(&self, config: Value) -> Diagnostics {
        match from_value::<S::Config>(&config) {
            Ok(cfg) => self.inner.validate(cfg).await,
            Err(e) => codec_diag("decode state store config for validation", e),
        }
    }

    async fn configure(&self, config: Value) -> Result<Arc<dyn DynStateBackend>, Diagnostics> {
        let cfg: S::Config =
            from_value(&config).map_err(|e| codec_diag("decode state store config", e))?;
        let backend = self
            .inner
            .configure(cfg)
            .await
            .map_err(Diag::from)
            .map_err(|d| vec![d])?;
        Ok(StateBackendAdapter::erased(backend))
    }
}

/// Wraps a typed [`StateBackend`] as an erased [`DynStateBackend`].
pub struct StateBackendAdapter<B: StateBackend> {
    inner: B,
}

impl<B: StateBackend> StateBackendAdapter<B> {
    /// Erase `backend` behind an `Arc<dyn DynStateBackend>`.
    pub fn erased(backend: B) -> Arc<dyn DynStateBackend> {
        Arc::new(StateBackendAdapter { inner: backend })
    }
}

/// Lift a [`StateStoreError`]-returning backend call into the erased
/// [`Diagnostics`] form.
fn into_diags<T>(r: Result<T, StateStoreError>) -> Result<T, Diagnostics> {
    r.map_err(Diag::from).map_err(|d| vec![d])
}

#[async_trait]
impl<B: StateBackend> DynStateBackend for StateBackendAdapter<B> {
    async fn read_state(&self, state_id: String) -> Result<Vec<u8>, Diagnostics> {
        into_diags(self.inner.read_state(state_id).await)
    }

    async fn write_state(&self, state_id: String, data: Vec<u8>) -> Result<(), Diagnostics> {
        into_diags(self.inner.write_state(state_id, data).await)
    }

    async fn lock(&self, state_id: String, operation: String) -> Result<String, Diagnostics> {
        into_diags(self.inner.lock(state_id, operation).await)
    }

    async fn unlock(&self, state_id: String, lock_id: String) -> Result<(), Diagnostics> {
        into_diags(self.inner.unlock(state_id, lock_id).await)
    }

    async fn states(&self) -> Result<Vec<String>, Diagnostics> {
        into_diags(self.inner.states().await)
    }

    async fn delete_state(&self, state_id: String) -> Result<(), Diagnostics> {
        into_diags(self.inner.delete_state(state_id).await)
    }
}
