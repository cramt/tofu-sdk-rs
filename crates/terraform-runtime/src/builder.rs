//! The provider definition and its builder.
//!
//! Authors describe their provider declaratively — registering a config type,
//! resources (each with a handler), and data sources — and the builder reflects
//! the schema up front while keeping the resource handlers for dispatch.

use std::collections::HashMap;
use std::sync::Arc;

use facet::Facet;
use terraform_ir::{Block, ProviderSchema};
use terraform_reflect::{reflect_block, reflect_data_source, reflect_resource, ReflectError};
use terraform_value::Type;

use crate::resource::{DynResource, Resource, ResourceAdapter};

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
#[derive(Clone)]
pub struct Provider {
    schema: Arc<ProviderSchema>,
    resources: Arc<HashMap<String, Arc<dyn DynResource>>>,
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

    /// The handler for resource type `name`, if registered.
    pub(crate) fn resource_handler(&self, name: &str) -> Option<&Arc<dyn DynResource>> {
        self.resources.get(name)
    }

    /// The `cty` object type of resource `name`, derived from its schema block.
    pub(crate) fn resource_cty(&self, name: &str) -> Option<Type> {
        self.resource_block(name).map(Block::cty_type)
    }

    /// The schema block of resource `name`.
    pub(crate) fn resource_block(&self, name: &str) -> Option<&Block> {
        self.schema
            .resources
            .iter()
            .find(|r| r.name == name)
            .map(|r| &r.block)
    }
}

/// Incremental builder for a [`Provider`].
#[derive(Default)]
pub struct ProviderBuilder {
    provider: Option<Block>,
    schema: ProviderSchema,
    resources: HashMap<String, Arc<dyn DynResource>>,
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

    /// Register a managed resource type under `name` with its `handler`.
    pub fn resource<R: Resource>(mut self, name: impl Into<String>, handler: R) -> Self {
        let name = name.into();
        match reflect_resource::<R::Model>(name.clone()) {
            Ok(resource) => {
                self.schema.resources.push(resource);
                self.resources
                    .insert(name, ResourceAdapter::erased(handler));
            }
            Err(source) => self.record(name, source),
        }
        self
    }

    /// Register a data source type under `name` (schema only for now).
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
            resources: Arc::new(self.resources),
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
