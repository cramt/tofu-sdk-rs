//! The provider definition and its builder.
//!
//! Authors describe their provider declaratively — registering a config type,
//! resources, and data sources, each by Rust type + Terraform type name — and
//! the builder reflects them into the backend-agnostic
//! [`terraform_ir::ProviderSchema`] up front. The resulting [`Provider`] is what
//! the gRPC service answers from.

use std::sync::Arc;

use facet::Facet;
use terraform_ir::{Block, ProviderSchema};
use terraform_reflect::{reflect_block, reflect_data_source, reflect_resource, ReflectError};

/// Error returned when a provider definition fails to build.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    /// A registered type could not be reflected into the IR.
    #[error("failed to reflect `{name}`: {source}")]
    Reflect {
        /// The Terraform type name (or `"provider"` for the config block).
        name: String,
        /// The underlying reflection error.
        #[source]
        source: ReflectError,
    },
}

/// A fully-described provider, ready to be served.
#[derive(Debug, Clone)]
pub struct Provider {
    schema: Arc<ProviderSchema>,
}

impl Provider {
    /// Start building a provider.
    pub fn builder() -> ProviderBuilder {
        ProviderBuilder::default()
    }

    /// The reflected provider IR.
    pub fn schema(&self) -> &ProviderSchema {
        &self.schema
    }
}

/// Incremental builder for a [`Provider`].
#[derive(Default)]
pub struct ProviderBuilder {
    provider: Option<Block>,
    schema: ProviderSchema,
    error: Option<BuildError>,
}

impl ProviderBuilder {
    /// Set the provider-level configuration block type.
    pub fn provider_config<T: Facet<'static>>(mut self) -> Self {
        match reflect_block::<T>() {
            Ok(block) => self.provider = Some(block),
            Err(source) => self.record("provider", source),
        }
        self
    }

    /// Register a managed resource type under `name` (e.g. `aws_s3_bucket`).
    pub fn resource<T: Facet<'static>>(mut self, name: impl Into<String>) -> Self {
        let name = name.into();
        match reflect_resource::<T>(name.clone()) {
            Ok(resource) => self.schema.resources.push(resource),
            Err(source) => self.record(name, source),
        }
        self
    }

    /// Register a data source type under `name`.
    pub fn data_source<T: Facet<'static>>(mut self, name: impl Into<String>) -> Self {
        let name = name.into();
        match reflect_data_source::<T>(name.clone()) {
            Ok(data_source) => self.schema.data_sources.push(data_source),
            Err(source) => self.record(name, source),
        }
        self
    }

    /// Finish building, returning the first reflection error if any occurred.
    pub fn build(mut self) -> Result<Provider, BuildError> {
        if let Some(err) = self.error.take() {
            return Err(err);
        }
        self.schema.provider = self.provider;
        Ok(Provider {
            schema: Arc::new(self.schema),
        })
    }

    /// Record the first error encountered (later errors are suppressed so the
    /// caller sees the root cause first).
    fn record(&mut self, name: impl Into<String>, source: ReflectError) {
        if self.error.is_none() {
            self.error = Some(BuildError::Reflect {
                name: name.into(),
                source,
            });
        }
    }
}
