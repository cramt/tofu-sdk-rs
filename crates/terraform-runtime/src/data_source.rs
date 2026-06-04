//! The author-facing data source traits and their internal type erasure.
//!
//! Data sources are read-only. There are two shapes, distinguished by how a
//! lookup key matches (the `search_key` cardinality reflected from the model):
//!
//! - [`DataSource`] — *singular*: a lookup by a unique (`exclusive`) key
//!   resolves to a single object. `read` returns one `Model`.
//! - [`DataSourceList`] — *plural*: a lookup by a generic (`shared`) key may
//!   match any number of objects. `list` returns a `Vec<Model>`, which the
//!   runtime wraps into the data source's `results` list.
//!
//! Authors implement these over a plain Rust `Model`; the runtime wraps each
//! handler in an adapter ([`DataSourceAdapter`] / [`DataSourceListAdapter`])
//! that bridges the dynamic [`Value`] and the typed `Model`. The erased
//! [`DynDataSource`] is what the gRPC service dispatches to for
//! `ReadDataSource`.

use std::collections::BTreeMap;
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
            attribute: Vec::new(),
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

    /// Validate the data source configuration, returning any diagnostics. Runs
    /// before reading; unset/unknown attributes arrive as their zero value.
    /// Defaults to no diagnostics.
    async fn validate(&self, _config: Self::Model) -> Vec<Diag> {
        Vec::new()
    }
}

/// Object-safe, value-oriented form of [`DataSource`] that the service
/// dispatches to. Operates on the dynamic [`Value`]; the [`DataSourceAdapter`]
/// bridges to the typed `Model`.
#[async_trait]
pub trait DynDataSource: Send + Sync {
    async fn read(&self, config: Value) -> Result<Value, Diagnostics>;
    async fn validate(&self, config: Value) -> Diagnostics;
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

    async fn validate(&self, config: Value) -> Diagnostics {
        match from_value::<D::Model>(&config) {
            Ok(model) => self.inner.validate(model).await,
            Err(e) => codec_diag("decode data source config for validation", e),
        }
    }
}

/// A plural data source: a lookup by a generic (`shared`) key that may match any
/// number of objects.
///
/// Implement this over the same `Model` as the resource. `list` receives a query
/// whose `search_key(shared)` fields are populated from the configuration (the
/// rest are their zero value) and returns every matching object. The runtime
/// wraps the results into the data source's computed `results` list.
#[async_trait]
pub trait DataSourceList: Send + Sync + 'static {
    /// The Rust type modeling this data source's schema (search keys + the shape
    /// of each result object).
    type Model: Facet<'static> + Send + Sync;

    /// Return every object matching the populated search keys in `query`.
    async fn list(&self, query: Self::Model) -> Result<Vec<Self::Model>, DataSourceError>;
}

/// Wraps a typed [`DataSourceList`] as an erased [`DynDataSource`]. It decodes
/// the query, calls `list`, and assembles the `{ <search keys>, results: [...] }`
/// wrapper object the plural schema expects.
pub struct DataSourceListAdapter<D: DataSourceList> {
    inner: D,
    /// The `search_key(shared)` field names, echoed back into the wrapper.
    shared_keys: Vec<String>,
}

impl<D: DataSourceList> DataSourceListAdapter<D> {
    /// Erase `data_source` (with its reflected shared-key names) behind an
    /// `Arc<dyn DynDataSource>`.
    pub fn erased(data_source: D, shared_keys: Vec<String>) -> Arc<dyn DynDataSource> {
        Arc::new(DataSourceListAdapter {
            inner: data_source,
            shared_keys,
        })
    }
}

#[async_trait]
impl<D: DataSourceList> DynDataSource for DataSourceListAdapter<D> {
    async fn read(&self, config: Value) -> Result<Value, Diagnostics> {
        // The plural config wraps the shared-key inputs (its `results` key, if
        // present, is not a model field and is ignored by the decode).
        let query: D::Model =
            from_value(&config).map_err(|e| codec_diag("decode data source query", e))?;
        let items = self
            .inner
            .list(query)
            .await
            .map_err(Diag::from)
            .map_err(|d| vec![d])?;

        let mut results = Vec::with_capacity(items.len());
        for item in &items {
            results.push(to_value(item).map_err(|e| codec_diag("encode data source result", e))?);
        }

        // Echo the search-key inputs back alongside the results list.
        let mut wrapper = BTreeMap::new();
        if let Value::Object(cfg) = &config {
            for key in &self.shared_keys {
                wrapper.insert(key.clone(), cfg.get(key).cloned().unwrap_or(Value::Null));
            }
        }
        wrapper.insert("results".to_string(), Value::List(results));
        Ok(Value::Object(wrapper))
    }

    async fn validate(&self, _config: Value) -> Diagnostics {
        // Plural data sources expose only search-key inputs; validation of the
        // query is left to the read handler for now.
        Vec::new()
    }
}
