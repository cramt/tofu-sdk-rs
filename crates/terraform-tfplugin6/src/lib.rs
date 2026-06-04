//! Terraform plugin protocol v6 (`tfplugin6`) backend.
//!
//! This is the Terraform-specific backend: it owns the generated protocol types
//! ([`tfplugin6`]) and the [`emit`]ter that lowers the backend-agnostic
//! [`terraform_ir`] into a Terraform [`tfplugin6::Schema`]. Keeping all
//! Terraform protocol concerns here is what lets the IR stay backend-neutral.

/// Generated Terraform plugin protocol v6 message types.
///
/// These prost-generated types are an implementation detail of the Terraform
/// backend and must never appear in the public provider API.
#[allow(clippy::all, clippy::pedantic, rustdoc::all, missing_docs)]
pub mod tfplugin6 {
    include!(concat!(env!("OUT_DIR"), "/tfplugin6.rs"));
}

mod emit;

pub use emit::{emit_block, emit_schema};
