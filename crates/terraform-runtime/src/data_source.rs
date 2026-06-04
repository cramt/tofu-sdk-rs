//! The author-facing [`DataSource`] trait and its internal type erasure.
//!
//! Data sources are read-only: given a configuration block, the provider
//! computes and returns a state. The shape mirrors [`crate::resource`] but with
//! a single `read` operation. Authors implement [`DataSource`] over a plain Rust
//! `Model`; the runtime wraps each handler in a [`DataSourceAdapter`] that
//! decodes the dynamic [`Value`] into the model, calls `read`, and encodes the
//! result back. The erased [`DynDataSource`] is what the gRPC service dispatches
//! to for `ReadDataSource`.

use std::sync::Arc;

use async_trait::async_trait;
use facet::Facet;
use terraform_codec::{from_value, to_value};
use terraform_value::Value;

use crate::resource::{codec_diag, Diag, Diagnostics, Severity};

/// An error returned by a data source read, surfaced to Terraform as an error
/// diagnostic.
#[derive(Debug, Clone)]
pub struct DataSourceError {
    /// Short, one-line summary.
    pub summary: String,
    /// Optional longer explanation.
    pub detail: String,
}

impl DataSourceError {
    /// Create an error with a summary.
    pub fn new(summary: impl Into<String>) -> Self {
        DataSourceError {
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

impl From<&str> for DataSourceError {
    fn from(s: &str) -> Self {
        DataSourceError::new(s)
    }
}

impl From<String> for DataSourceError {
    fn from(s: String) -> Self {
        DataSourceError::new(s)
    }
}

impl From<DataSourceError> for Diag {
    fn from(e: DataSourceError) -> Self {
        Diag {
            severity: Severity::Error,
            summary: e.summary,
            detail: e.detail,
        }
    }
}

/// A read-only data source type.
///
/// Implement this over a `Model` struct that reflects (via `#[derive(Facet)]`)
/// the data source's schema. The `read` method receives the configured
/// arguments (computed attributes arrive as their zero value) and returns the
/// fully-populated state.
#[async_trait]
pub trait DataSource: Send + Sync + 'static {
    /// The Rust type modeling this data source's schema (arguments + computed
    /// results).
    type Model: Facet<'static> + Send + Sync;

    /// Read the data source from its configuration and return the state with
    /// computed attributes filled in.
    async fn read(&self, config: Self::Model) -> Result<Self::Model, DataSourceError>;
}

/// Object-safe, value-oriented form of [`DataSource`] that the service
/// dispatches to. Operates on the dynamic [`Value`]; the [`DataSourceAdapter`]
/// bridges to the typed `Model`.
#[async_trait]
pub trait DynDataSource: Send + Sync {
    async fn read(&self, config: Value) -> Result<Value, Diagnostics>;
}

/// Wraps a typed [`DataSource`] as an erased [`DynDataSource`].
pub struct DataSourceAdapter<D: DataSource> {
    inner: D,
}

impl<D: DataSource> DataSourceAdapter<D> {
    /// Erase `data_source` behind an `Arc<dyn DynDataSource>`.
    pub fn erased(data_source: D) -> Arc<dyn DynDataSource> {
        Arc::new(DataSourceAdapter { inner: data_source })
    }
}

#[async_trait]
impl<D: DataSource> DynDataSource for DataSourceAdapter<D> {
    async fn read(&self, config: Value) -> Result<Value, Diagnostics> {
        let model: D::Model =
            from_value(&config).map_err(|e| codec_diag("decode data source config", e))?;
        let result = self
            .inner
            .read(model)
            .await
            .map_err(Diag::from)
            .map_err(|d| vec![d])?;
        to_value(&result).map_err(|e| codec_diag("encode data source state", e))
    }
}
