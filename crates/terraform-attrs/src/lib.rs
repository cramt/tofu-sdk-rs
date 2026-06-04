//! The `terraform` facet extension-attribute namespace.
//!
//! Provider authors attach Terraform semantics to their Rust types with
//! `#[facet(terraform::...)]`. Bring the namespace into scope by aliasing this
//! crate (the facet convention — see `use facet_xml as xml`):
//!
//! ```ignore
//! use terraform_attrs as terraform; // or: use terraform_provider::terraform;
//! use facet::Facet;
//!
//! #[derive(Facet)]
//! #[facet(terraform::resource)]
//! struct Bucket {
//!     #[facet(terraform::required)]
//!     #[facet(terraform::force_new)]
//!     name: String,
//!     #[facet(terraform::computed)]
//!     arn: String,
//! }
//! ```
//!
//! The metadata is read back at runtime by `terraform-reflect` via
//! `field.get_attr(Some("terraform"), "<key>")` — it does not need to name the
//! [`Attr`] type directly, which keeps this crate a pure declaration.

facet::define_attr_grammar! {
    ns "terraform";
    crate_path ::terraform_attrs;

    /// Semantic Terraform metadata attachable to fields and containers.
    ///
    /// Variant names map to snake_case attribute keys: `ForceNew` becomes
    /// `#[facet(terraform::force_new)]`.
    pub enum Attr {
        /// The caller must set this attribute.
        Required,
        /// The caller may set this attribute.
        Optional,
        /// The provider computes this attribute; it may be unknown at plan time.
        Computed,
        /// Changing this attribute forces resource replacement.
        ForceNew,
        /// The value is sensitive and should be redacted.
        Sensitive,
        /// Marks a struct as a managed resource.
        Resource,
        /// Marks a struct as a data source.
        DataSource,
    }
}
