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
pub use service::ProviderService;

/// Re-export of `async_trait` so authors can `#[terraform_runtime::async_trait]`
/// their `impl Resource`.
pub use async_trait::async_trait;
