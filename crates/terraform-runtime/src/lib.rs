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
mod ctx;
mod data_source;
mod ephemeral;
mod function;
pub mod handshake;
mod list;
mod log;
// Semantic equality (roadmap 3.6): diff suppression derived from a field's
// quotient type, auto-harvested from the model's `SHAPE` (see `normalize.rs`).
mod normalize;
mod plan;
mod resource;
mod serve;
mod service;
mod timeouts;
mod tls;
mod write_only;

pub use builder::{
    BuildError, ConfigureError, DynConfigure, DynValidateConfig, IntoConfigured, Provider,
    ProviderBuilder,
};
pub use ctx::Ctx;
pub use data_source::{DataSource, DataSourceError, DataSourceList};
pub use ephemeral::{Ephemeral, EphemeralError, EphemeralFromResource};
pub use function::{Function, FunctionError, VariadicFunction};
pub use list::{ListError, ListItem, ListResource};
pub use normalize::{string_quotient, Canon};
pub use resource::{Path, PathStep, PlanModifications, Resource, ResourceError};
pub use serve::{serve, ServeError};
pub use timeouts::Timeouts;

/// The erased, `Value`-oriented handler traits and diagnostic types. These are
/// the dynamic seam used by non-Rust frontends (paired with
/// [`ProviderBuilder::dyn_resource`] / [`ProviderBuilder::dyn_data_source`]);
/// Rust authors use the typed [`Resource`] / [`DataSource`] traits instead.
pub use ctx::current_ctx;
pub use data_source::DynDataSource;
pub use ephemeral::DynEphemeral;
pub use function::DynFunction;
pub use list::{DynListItem, DynListResource};
pub use resource::{Diag, Diagnostics, DynResource, Severity};
pub use service::{current_cancellation, ProviderService};

/// Re-export of `tokio_util`'s [`CancellationToken`](tokio_util::sync::CancellationToken)
/// so handlers can type the token returned by [`current_cancellation`] (e.g. to
/// `select!` on `token.cancelled()` and abort promptly on `StopProvider`).
pub use tokio_util::sync::CancellationToken;

/// Re-export of `async_trait` so authors can `#[terraform_runtime::async_trait]`
/// their `impl Resource`.
pub use async_trait::async_trait;
