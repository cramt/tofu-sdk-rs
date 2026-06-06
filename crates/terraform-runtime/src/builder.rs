//! The provider definition and its builder.
//!
//! Authors describe their provider declaratively — registering a config type,
//! resources (each with a handler), and data sources — and the builder reflects
//! the schema up front while keeping the resource handlers for dispatch.
//!
//! ## Provider configuration
//!
//! A provider can carry configuration (credentials, region, …). The author
//! registers the config type with [`ProviderBuilder::provider_config`] and a
//! [`ProviderBuilder::configure`] closure that turns the decoded config into
//! shared state — typically an API client. That shared state (the *meta*) is an
//! `Arc<M>` handed to every resource registered with
//! [`ProviderBuilder::resource_with`], so handlers keep their plain typed CRUD
//! signatures and reach the configured client through `self`.
//!
//! The meta is built lazily, once, when Terraform calls `ConfigureProvider`.
//! Resources that need no configuration are still registered eagerly with
//! [`ProviderBuilder::resource`].

use std::collections::HashMap;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;

use facet::Facet;
use terraform_codec::from_value;
use terraform_ir::{Block, DataSourceSchema, ProviderSchema, ResourceSchema};
use terraform_reflect::{
    data_source_list_name, data_source_name, reflect_block, reflect_data_source,
    reflect_data_source_list, reflect_resource, resource_name, PluralDataSource, ReflectError,
};
use terraform_value::{Type, Value};
use tokio::sync::OnceCell;

use async_trait::async_trait;

use crate::data_source::{
    DataSource, DataSourceAdapter, DataSourceList, DataSourceListAdapter, DynDataSource,
};
use crate::resource::{Diag, Diagnostics, DynResource, Resource, ResourceAdapter};

/// An erased provider-configure callback, the dynamic-seam counterpart to
/// [`ProviderBuilder::configure`]. It runs side effects from the decoded provider
/// config (e.g. a non-Rust frontend setting up its own client/state); unlike the
/// typed `configure`, it produces no Rust meta. Paired with
/// [`ProviderBuilder::dyn_provider_config`].
#[async_trait]
pub trait DynConfigure: Send + Sync {
    async fn configure(&self, config: Value) -> Result<(), Diagnostics>;
}

/// An erased provider-config validation callback, the dynamic-seam counterpart to
/// [`ProviderBuilder::validate_config`]. It runs in `ValidateProviderConfig`
/// (before `ConfigureProvider`) and returns diagnostics for a bad provider block.
/// Paired with [`ProviderBuilder::dyn_validate_config`].
#[async_trait]
pub trait DynValidateConfig: Send + Sync {
    async fn validate(&self, config: Value) -> Diagnostics;
}

/// Bridges a typed `Fn(C) -> Future<Output = Diagnostics>` closure to the erased
/// [`DynValidateConfig`], decoding the dynamic [`Value`] into `C` first.
struct TypedValidateConfig<C, F> {
    f: F,
    _marker: PhantomData<fn(C)>,
}

#[async_trait]
impl<C, F, Fut> DynValidateConfig for TypedValidateConfig<C, F>
where
    C: Facet<'static> + Send,
    F: Fn(C) -> Fut + Send + Sync,
    Fut: Future<Output = Diagnostics> + Send,
{
    async fn validate(&self, config: Value) -> Diagnostics {
        match from_value::<C>(&config) {
            Ok(cfg) => (self.f)(cfg).await,
            Err(e) => vec![Diag::error(
                "failed to decode provider config for validation",
                e.to_string(),
            )],
        }
    }
}

/// An error returned by a provider [`ProviderBuilder::configure`] closure,
/// surfaced to Terraform as a configuration diagnostic (e.g. bad credentials or
/// an unreachable endpoint).
#[derive(Debug, Clone)]
pub struct ConfigureError {
    /// Short, one-line summary.
    pub summary: String,
    /// Optional longer explanation.
    pub detail: String,
}

impl ConfigureError {
    /// Create an error with a summary.
    pub fn new(summary: impl Into<String>) -> Self {
        ConfigureError {
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

impl From<&str> for ConfigureError {
    fn from(s: &str) -> Self {
        ConfigureError::new(s)
    }
}

impl From<String> for ConfigureError {
    fn from(s: String) -> Self {
        ConfigureError::new(s)
    }
}

impl From<ConfigureError> for Diag {
    fn from(e: ConfigureError) -> Self {
        Diag::error(e.summary, e.detail)
    }
}

/// Conversion from a [`ProviderBuilder::configure`] closure's output into the
/// shared meta (or a diagnostic). Implemented for both the infallible `Arc<M>`
/// (always succeeds) and the fallible `Result<Arc<M>, E>` for any `E: Into<Diag>`
/// (e.g. [`ConfigureError`]), so an author can write either
/// `async { Arc::new(client) }` or `async { Ok(Arc::new(connect().await?)) }`.
pub trait IntoConfigured<M> {
    /// Resolve to the shared meta, mapping any error into diagnostics.
    fn into_configured(self) -> Result<Arc<M>, Diagnostics>;
}

impl<M> IntoConfigured<M> for Arc<M> {
    fn into_configured(self) -> Result<Arc<M>, Diagnostics> {
        Ok(self)
    }
}

impl<M, E: Into<Diag>> IntoConfigured<M> for Result<Arc<M>, E> {
    fn into_configured(self) -> Result<Arc<M>, Diagnostics> {
        self.map_err(|e| vec![e.into()])
    }
}

/// A `Send` boxed future, the erased form of a handler/closure's async result.
type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// The set of resource handlers keyed by type name.
type ResourceMap = HashMap<String, Arc<dyn DynResource>>;

/// The set of data source handlers keyed by type name.
type DataSourceMap = HashMap<String, Arc<dyn DynDataSource>>;

/// The meta-backed handlers built once `ConfigureProvider` runs.
#[derive(Default)]
struct Configured {
    resources: ResourceMap,
    data_sources: DataSourceMap,
}

/// Builds the shared meta `Arc<M>` from the decoded provider config value.
type MetaFn<M> = Arc<dyn Fn(Value) -> BoxFuture<Result<Arc<M>, Diagnostics>> + Send + Sync>;

/// Builds one resource handler from the shared meta. Stored per registered
/// `resource_with` until the meta exists at configure time.
type ResourceFactory<M> = Box<dyn Fn(Arc<M>) -> Arc<dyn DynResource> + Send + Sync>;

/// Builds one data source handler from the shared meta. Stored per registered
/// `data_source_with` until the meta exists at configure time.
type DataSourceFactory<M> = Box<dyn Fn(Arc<M>) -> Arc<dyn DynDataSource> + Send + Sync>;

/// The fully-erased configure step: decode-and-build the meta, then construct
/// every meta-backed handler. `M` has been erased away by this point.
type ConfigureFn = dyn Fn(Value) -> BoxFuture<Result<Configured, Diagnostics>> + Send + Sync;

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

    /// A resource was registered with [`ProviderBuilder::resource_with`] but the
    /// builder has no [`ProviderBuilder::configure`] step to produce the meta it
    /// depends on.
    #[error("a resource needs provider meta but no `configure` step was registered")]
    MissingConfigure,
}

/// A fully-described provider, ready to be served.
#[derive(Clone)]
pub struct Provider {
    schema: Arc<ProviderSchema>,
    /// Resource handlers that need no provider meta; available immediately.
    eager: Arc<ResourceMap>,
    /// Data source handlers that need no provider meta; available immediately.
    eager_data: Arc<DataSourceMap>,
    /// The `cty` type of the provider config block, for decoding `ConfigureProvider`.
    config_ty: Option<Type>,
    /// Builds the meta and the meta-backed handlers from decoded config. `None`
    /// when the provider has no `configure` step.
    configure: Option<Arc<ConfigureFn>>,
    /// The meta-backed handlers, populated once `ConfigureProvider` runs.
    configured: Arc<OnceCell<Configured>>,
    /// A dynamic-seam configure callback (e.g. the Node binding), run on
    /// `ConfigureProvider`. Independent of the typed `configure` above.
    dyn_configure: Option<Arc<dyn DynConfigure>>,
    /// A provider-config validation callback, run on `ValidateProviderConfig`
    /// (before configure). `None` validates clean.
    validate_config: Option<Arc<dyn DynValidateConfig>>,
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

    /// The handler for resource type `name`, if registered. Configured
    /// (meta-backed) handlers take precedence over eager ones.
    pub(crate) fn resource_handler(&self, name: &str) -> Option<Arc<dyn DynResource>> {
        if let Some(configured) = self.configured.get() {
            if let Some(handler) = configured.resources.get(name) {
                return Some(Arc::clone(handler));
            }
        }
        self.eager.get(name).map(Arc::clone)
    }

    /// The handler for data source type `name`, if registered. Configured
    /// (meta-backed) handlers take precedence over eager ones.
    pub(crate) fn data_source_handler(&self, name: &str) -> Option<Arc<dyn DynDataSource>> {
        if let Some(configured) = self.configured.get() {
            if let Some(handler) = configured.data_sources.get(name) {
                return Some(Arc::clone(handler));
            }
        }
        self.eager_data.get(name).map(Arc::clone)
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

    /// The current state-schema version of resource `name`.
    pub(crate) fn resource_version(&self, name: &str) -> Option<i64> {
        self.schema
            .resources
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.version)
    }

    /// The `cty` object type of data source `name`, derived from its schema block.
    pub(crate) fn data_source_cty(&self, name: &str) -> Option<Type> {
        self.schema
            .data_sources
            .iter()
            .find(|d| d.name == name)
            .map(|d| d.block.cty_type())
    }

    /// The `cty` type Terraform's `ConfigureProvider` config decodes under. An
    /// empty object when the provider declares no configuration.
    pub(crate) fn provider_config_ty(&self) -> Type {
        self.config_ty
            .clone()
            .unwrap_or_else(|| Type::Object(Vec::new()))
    }

    /// Validate the decoded provider `config`, returning any diagnostics. Runs in
    /// `ValidateProviderConfig`, before configure. No-op (clean) when the provider
    /// registered no validation callback.
    pub(crate) async fn validate_config(&self, config: Value) -> Diagnostics {
        match &self.validate_config {
            Some(handler) => handler.validate(config).await,
            None => Vec::new(),
        }
    }

    /// Run the provider's configure step with the decoded `config`, building the
    /// shared meta and meta-backed handlers exactly once. A no-op (and `Ok`) for
    /// providers without a `configure` step.
    pub(crate) async fn configure(&self, config: Value) -> Result<(), Diagnostics> {
        // Dynamic-seam callback first (it sets up the frontend's own state that
        // the eager handlers read), then the typed meta build, if any.
        if let Some(dyn_configure) = &self.dyn_configure {
            dyn_configure.configure(config.clone()).await?;
        }
        if let Some(configure) = self.configure.clone() {
            self.configured
                .get_or_try_init(|| configure(config))
                .await?;
        }
        Ok(())
    }
}

/// Incremental builder for a [`Provider`], parameterized by the provider's
/// shared meta type `M` (`()` until [`ProviderBuilder::configure`] is called).
pub struct ProviderBuilder<M = ()> {
    provider: Option<Block>,
    schema: ProviderSchema,
    resources: ResourceMap,
    data_sources: DataSourceMap,
    factories: Vec<(String, ResourceFactory<M>)>,
    data_factories: Vec<(String, DataSourceFactory<M>)>,
    configure: Option<MetaFn<M>>,
    dyn_configure: Option<Arc<dyn DynConfigure>>,
    validate_config: Option<Arc<dyn DynValidateConfig>>,
    error: Option<BuildError>,
}

impl<M> Default for ProviderBuilder<M> {
    fn default() -> Self {
        ProviderBuilder {
            provider: None,
            schema: ProviderSchema::default(),
            resources: HashMap::new(),
            data_sources: HashMap::new(),
            factories: Vec::new(),
            data_factories: Vec::new(),
            configure: None,
            dyn_configure: None,
            validate_config: None,
            error: None,
        }
    }
}

impl<M: Send + Sync + 'static> ProviderBuilder<M> {
    /// Set the provider-level configuration block type.
    pub fn provider_config<T: Facet<'static>>(mut self) -> Self {
        match reflect_block::<T>() {
            Ok(block) => self.provider = Some(block),
            Err(source) => self.record("provider", source),
        }
        self
    }

    /// Register a managed resource type under `name` with its `handler`. Use this
    /// for resources that need no provider configuration; for resources that need
    /// the configured meta, use [`ProviderBuilder::resource_with`].
    ///
    /// The type name comes from the model: an explicit
    /// `#[facet(terraform::resource("name"))]`, or `snake_case` of the struct
    /// identifier when none is given.
    pub fn resource<R: Resource>(mut self, handler: R) -> Self {
        let name = resource_name::<R::Model>();
        match reflect_resource::<R::Model>(name.clone()) {
            Ok(mut resource) => {
                resource.version = R::SCHEMA_VERSION;
                self.schema.resources.push(resource);
                self.resources
                    .insert(name, ResourceAdapter::erased(handler));
            }
            Err(source) => self.record(name, source),
        }
        self
    }

    /// Register a managed resource whose handler is built from the configured
    /// provider meta. `factory` receives the shared `Arc<M>` produced by
    /// [`ProviderBuilder::configure`] and returns the resource handler.
    ///
    /// The type name comes from the model (see [`ProviderBuilder::resource`]).
    /// Requires a [`ProviderBuilder::configure`] step (which fixes `M`); building
    /// without one is a [`BuildError::MissingConfigure`].
    pub fn resource_with<R, F>(mut self, factory: F) -> Self
    where
        R: Resource,
        F: Fn(Arc<M>) -> R + Send + Sync + 'static,
    {
        let name = resource_name::<R::Model>();
        match reflect_resource::<R::Model>(name.clone()) {
            Ok(mut resource) => {
                resource.version = R::SCHEMA_VERSION;
                self.schema.resources.push(resource);
                let make: ResourceFactory<M> =
                    Box::new(move |meta: Arc<M>| ResourceAdapter::erased(factory(meta)));
                self.factories.push((name, make));
            }
            Err(source) => self.record(name, source),
        }
        self
    }

    /// Register a read-only (singular) data source with its `handler`.
    /// Use this for data sources that need no provider configuration; for ones
    /// that need the configured meta, use [`ProviderBuilder::data_source_with`].
    ///
    /// The type name comes from the model: an explicit
    /// `#[facet(terraform::data_source("name"))]`, or `snake_case` of the struct
    /// identifier when none is given.
    pub fn data_source<D: DataSource>(mut self, handler: D) -> Self {
        let name = data_source_name::<D::Model>();
        match reflect_data_source::<D::Model>(name.clone()) {
            Ok(data_source) => {
                self.schema.data_sources.push(data_source);
                self.data_sources
                    .insert(name, DataSourceAdapter::erased(handler));
            }
            Err(source) => self.record(name, source),
        }
        self
    }

    /// Register a data source whose handler is built from the configured
    /// provider meta. `factory` receives the shared `Arc<M>` produced by
    /// [`ProviderBuilder::configure`] and returns the data source handler.
    ///
    /// The type name comes from the model (see [`ProviderBuilder::data_source`]).
    /// Requires a [`ProviderBuilder::configure`] step (which fixes `M`); building
    /// without one is a [`BuildError::MissingConfigure`].
    pub fn data_source_with<D, F>(mut self, factory: F) -> Self
    where
        D: DataSource,
        F: Fn(Arc<M>) -> D + Send + Sync + 'static,
    {
        let name = data_source_name::<D::Model>();
        match reflect_data_source::<D::Model>(name.clone()) {
            Ok(data_source) => {
                self.schema.data_sources.push(data_source);
                let make: DataSourceFactory<M> =
                    Box::new(move |meta: Arc<M>| DataSourceAdapter::erased(factory(meta)));
                self.data_factories.push((name, make));
            }
            Err(source) => self.record(name, source),
        }
        self
    }

    /// Register a **plural** data source with its `handler`: a lookup by
    /// `search_key(shared)` fields that resolves to a `results` list. Use this for
    /// data sources that need no provider configuration; for ones that need the
    /// configured meta, use [`ProviderBuilder::data_source_list_with`].
    ///
    /// The type name is the singular data-source name with an `s` appended
    /// (`aws_s3_bucket` → `aws_s3_buckets`), so one model can back both a singular
    /// and a plural data source.
    pub fn data_source_list<D: DataSourceList>(mut self, handler: D) -> Self {
        let name = data_source_list_name::<D::Model>();
        match reflect_data_source_list::<D::Model>(name.clone()) {
            Ok(PluralDataSource {
                schema,
                shared_keys,
            }) => {
                self.schema.data_sources.push(schema);
                self.data_sources
                    .insert(name, DataSourceListAdapter::erased(handler, shared_keys));
            }
            Err(source) => self.record(name, source),
        }
        self
    }

    /// Register a **plural** data source whose handler is built from the
    /// configured provider meta. `factory` receives the shared `Arc<M>` produced
    /// by [`ProviderBuilder::configure`] and returns the data source handler.
    ///
    /// The type name is the singular name plus `s` (see
    /// [`ProviderBuilder::data_source_list`]). Requires a
    /// [`ProviderBuilder::configure`] step (which fixes `M`); building without one
    /// is a [`BuildError::MissingConfigure`].
    pub fn data_source_list_with<D, F>(mut self, factory: F) -> Self
    where
        D: DataSourceList,
        F: Fn(Arc<M>) -> D + Send + Sync + 'static,
    {
        let name = data_source_list_name::<D::Model>();
        match reflect_data_source_list::<D::Model>(name.clone()) {
            Ok(PluralDataSource {
                schema,
                shared_keys,
            }) => {
                self.schema.data_sources.push(schema);
                let make: DataSourceFactory<M> = Box::new(move |meta: Arc<M>| {
                    DataSourceListAdapter::erased(factory(meta), shared_keys.clone())
                });
                self.data_factories.push((name, make));
            }
            Err(source) => self.record(name, source),
        }
        self
    }

    // --- Dynamic seam ------------------------------------------------------
    //
    // The methods above are the facet *frontend*: a Rust `Model` is reflected
    // into the IR and bridged to the dynamic `Value` by an adapter. The two
    // below skip reflection entirely — the caller supplies a hand-built schema
    // [`Block`] and an already-erased handler that operates on `Value`. This is
    // the seam non-Rust frontends use (e.g. the Node binding builds the IR from
    // a JS schema description and implements [`DynResource`] by calling into JS).

    /// Set the provider-level configuration block from a hand-built schema,
    /// bypassing facet reflection. Pair with [`ProviderBuilder::dyn_configure`]
    /// to receive the decoded config on `ConfigureProvider`.
    pub fn dyn_provider_config(mut self, block: Block) -> Self {
        self.provider = Some(block);
        self
    }

    /// Register a dynamic-seam configure callback, run with the decoded provider
    /// config on `ConfigureProvider`. The dynamic counterpart to
    /// [`ProviderBuilder::configure`] — it produces no Rust meta (a non-Rust
    /// frontend manages its own state).
    pub fn dyn_configure(mut self, handler: Arc<dyn DynConfigure>) -> Self {
        self.dyn_configure = Some(handler);
        self
    }

    /// Register a provider-config validation callback: a closure decoding the
    /// provider block into `C` and returning diagnostics (errors and/or warnings,
    /// optionally pointed at an attribute via [`Diag::at`]). Runs in
    /// `ValidateProviderConfig`, before configure — so it sees the raw config and
    /// must not assume any configured state. Mirrors [`Resource::validate`] for
    /// the provider block. Pair with [`ProviderBuilder::provider_config`] to
    /// publish `C`'s schema.
    ///
    /// [`Resource::validate`]: crate::Resource::validate
    pub fn validate_config<C, F, Fut>(mut self, f: F) -> Self
    where
        C: Facet<'static> + Send,
        F: Fn(C) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Diagnostics> + Send + 'static,
    {
        self.validate_config = Some(Arc::new(TypedValidateConfig {
            f,
            _marker: PhantomData,
        }));
        self
    }

    /// Register a dynamic-seam provider-config validation callback, run with the
    /// decoded config in `ValidateProviderConfig`. The dynamic counterpart to
    /// [`ProviderBuilder::validate_config`] (e.g. the Node binding validating its
    /// own schema).
    pub fn dyn_validate_config(mut self, handler: Arc<dyn DynValidateConfig>) -> Self {
        self.validate_config = Some(handler);
        self
    }

    /// Register a managed resource from a hand-built schema `block` and an
    /// erased [`DynResource`] handler, bypassing facet reflection.
    pub fn dyn_resource(
        mut self,
        name: impl Into<String>,
        version: i64,
        block: Block,
        handler: Arc<dyn DynResource>,
    ) -> Self {
        let name = name.into();
        self.schema.resources.push(ResourceSchema {
            name: name.clone(),
            version,
            block,
        });
        self.resources.insert(name, handler);
        self
    }

    /// Register a data source from a hand-built schema `block` and an erased
    /// [`DynDataSource`] handler, bypassing facet reflection. The `block` and
    /// handler together decide the shape (a singular object, or a plural
    /// `results` list); the runtime does not distinguish them here.
    pub fn dyn_data_source(
        mut self,
        name: impl Into<String>,
        block: Block,
        handler: Arc<dyn DynDataSource>,
    ) -> Self {
        let name = name.into();
        self.schema.data_sources.push(DataSourceSchema {
            name: name.clone(),
            block,
        });
        self.data_sources.insert(name, handler);
        self
    }

    /// Finish building, returning the first reflection error if any occurred.
    pub fn build(mut self) -> Result<Provider, BuildError> {
        if let Some(err) = self.error.take() {
            return Err(err);
        }
        self.schema.provider = self.provider.take();
        let config_ty = self.schema.provider.as_ref().map(Block::cty_type);

        let configure: Option<Arc<ConfigureFn>> = match self.configure.take() {
            Some(meta_fn) => {
                let factories = Arc::new(self.factories);
                let data_factories = Arc::new(self.data_factories);
                let configure: Arc<ConfigureFn> = Arc::new(move |config: Value| {
                    let meta_fn = Arc::clone(&meta_fn);
                    let factories = Arc::clone(&factories);
                    let data_factories = Arc::clone(&data_factories);
                    Box::pin(async move {
                        let meta = meta_fn(config).await?;
                        let mut resources = ResourceMap::with_capacity(factories.len());
                        for (name, make) in factories.iter() {
                            resources.insert(name.clone(), make(Arc::clone(&meta)));
                        }
                        let mut data_sources = DataSourceMap::with_capacity(data_factories.len());
                        for (name, make) in data_factories.iter() {
                            data_sources.insert(name.clone(), make(Arc::clone(&meta)));
                        }
                        Ok(Configured {
                            resources,
                            data_sources,
                        })
                    }) as BoxFuture<Result<Configured, Diagnostics>>
                });
                Some(configure)
            }
            // Meta-backed handlers were registered but there is nothing to build
            // their meta — a programming error worth surfacing at build time.
            None if !self.factories.is_empty() || !self.data_factories.is_empty() => {
                return Err(BuildError::MissingConfigure)
            }
            None => None,
        };

        Ok(Provider {
            schema: Arc::new(self.schema),
            eager: Arc::new(self.resources),
            eager_data: Arc::new(self.data_sources),
            config_ty,
            configure,
            configured: Arc::new(OnceCell::new()),
            dyn_configure: self.dyn_configure,
            validate_config: self.validate_config,
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

impl ProviderBuilder<()> {
    /// Register the provider's configure step: a closure that turns the decoded
    /// provider config `C` into shared meta `Arc<M>` (typically an API client).
    /// This fixes the meta type for subsequent [`ProviderBuilder::resource_with`]
    /// calls and runs once, when Terraform calls `ConfigureProvider`.
    ///
    /// Call [`ProviderBuilder::provider_config`] as well to publish `C`'s schema;
    /// `configure` only wires the runtime behavior.
    /// The closure may be infallible (returning `Arc<M>`) or fallible (returning
    /// `Result<Arc<M>, E>` for any `E: Into<Diag>`, e.g. [`ConfigureError`]); a
    /// returned error becomes a configuration diagnostic and aborts configure.
    pub fn configure<C, M, F, Fut, O>(self, f: F) -> ProviderBuilder<M>
    where
        C: Facet<'static>,
        M: Send + Sync + 'static,
        F: Fn(C) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = O> + Send + 'static,
        O: IntoConfigured<M> + Send + 'static,
    {
        let f = Arc::new(f);
        let meta_fn: MetaFn<M> = Arc::new(move |config: Value| {
            let f = Arc::clone(&f);
            Box::pin(async move {
                let cfg: C = from_value(&config)
                    .map_err(|e| vec![Diag::error("decode provider config", e.to_string())])?;
                f(cfg).await.into_configured()
            }) as BoxFuture<Result<Arc<M>, Diagnostics>>
        });

        // Carry the schema and eager handlers over to the meta-typed builder.
        // No `*_with` factories can exist yet (this is the only call that fixes
        // `M`), so there is nothing of the old `()` factory type to migrate.
        ProviderBuilder {
            provider: self.provider,
            schema: self.schema,
            resources: self.resources,
            data_sources: self.data_sources,
            factories: Vec::new(),
            data_factories: Vec::new(),
            configure: Some(meta_fn),
            dyn_configure: self.dyn_configure,
            validate_config: self.validate_config,
            error: self.error,
        }
    }
}
