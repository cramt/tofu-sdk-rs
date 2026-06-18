//! Public, user-facing provider API.
//!
//! Provider authors depend on this crate. It re-exports the derive, the
//! `terraform::*` attribute namespace, the provider IR types, and the reflection
//! entry points. Generated Terraform protocol types never appear in this surface
//! — emitting them is the job of an internal backend crate.
//!
//! ```
//! use terraform_provider::{terraform, Facet};
//! use terraform_provider::reflect_resource;
//!
//! #[derive(Facet)]
//! #[facet(terraform::resource)]
//! struct Bucket {
//!     // non-`Option` ⇒ inferred required; no `required` marker needed
//!     #[facet(terraform::force_new)]
//!     name: String,
//!     #[facet(terraform::computed)]
//!     arn: String,
//! }
//!
//! let resource = reflect_resource::<Bucket>("aws_s3_bucket").unwrap();
//! assert_eq!(resource.name, "aws_s3_bucket");
//! assert_eq!(resource.block.attributes.len(), 2);
//! ```

/// Re-export of facet's derive so authors write `use terraform_provider::Facet;`.
pub use facet::Facet;

/// The `#[facet(terraform::...)]` attribute namespace.
///
/// Authors bring this into scope as `terraform` (the facet convention) so they
/// can write `#[facet(terraform::computed)]`:
///
/// ```ignore
/// use terraform_provider::terraform;
/// ```
pub use terraform_attrs as terraform;

/// Backend-agnostic provider IR.
pub use terraform_ir as ir;

/// Terraform value semantics (the `cty` type system and known/unknown/null).
pub use terraform_value as value;

/// The known/unknown/null field wrapper. Use `TfValue<T>` for a model field that
/// must preserve Terraform's "unknown" (computed, not yet resolved) distinctly
/// from "null" — a plain `T` decodes both to its zero value.
pub use terraform_value::TfValue;

/// Reflection entry points: Rust types -> provider IR.
pub use terraform_reflect::{
    data_source_list_name, data_source_name, reflect_block, reflect_data_source,
    reflect_data_source_list, reflect_function, reflect_resource, reflect_variadic_function,
    resource_name, PluralDataSource, ReflectError,
};
