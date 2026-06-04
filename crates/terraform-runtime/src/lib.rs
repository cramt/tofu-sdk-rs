//! Provider runtime: serve a reflected provider over the Terraform plugin
//! protocol.
//!
//! ```text
//! Terraform Core ──gRPC (tfplugin6)──▶ ProviderService ──▶ reflected IR
//! ```
//!
//! Phase 2 wires up the schema-discovery RPCs, the go-plugin handshake, and
//! auto-mTLS. Resource CRUD and planning arrive in later phases.

mod builder;
mod data_source;
pub mod handshake;
mod plan;
mod resource;
mod serve;
mod service;
mod tls;

pub use builder::{BuildError, Provider, ProviderBuilder};
pub use data_source::{DataSource, DataSourceError, DataSourceList};
pub use resource::{Resource, ResourceError};
pub use serve::{serve, ServeError};

/// The erased, `Value`-oriented handler traits and diagnostic types. These are
/// the dynamic seam used by non-Rust frontends (paired with
/// [`ProviderBuilder::dyn_resource`] / [`ProviderBuilder::dyn_data_source`]);
/// Rust authors use the typed [`Resource`] / [`DataSource`] traits instead.
pub use data_source::DynDataSource;
pub use resource::{Diag, Diagnostics, DynResource, Severity};
pub use service::ProviderService;

/// Re-export of `async_trait` so authors can `#[terraform_runtime::async_trait]`
/// their `impl Resource`.
pub use async_trait::async_trait;
