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
pub mod handshake;
mod serve;
mod service;
mod tls;

pub use builder::{BuildError, Provider, ProviderBuilder};
pub use serve::{serve, ServeError};
pub use service::ProviderService;
