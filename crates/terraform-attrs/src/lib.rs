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
//! Unit flags are read back at runtime by `terraform-reflect` via
//! `field.has_attr(Some("terraform"), "<key>")`; the structured [`SearchKey`]
//! payload is decoded with
//! `field.get_attr(Some("terraform"), "search_key").get_as::<Attr>()`.

// The grammar's struct-payload codegen refers to this crate by its own path
// (`crate_path` below), so the crate must be able to name itself.
extern crate self as terraform_attrs;

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
        /// Render this field as a nested *block* (HCL `name { … }` syntax) rather
        /// than an attribute. The field's Rust type fixes the nesting mode:
        /// a struct (or `Option<struct>`) is a single block, `Vec<struct>` a
        /// list, a set a set, and `HashMap<String, struct>` a map. The element
        /// struct is reflected recursively, so blocks may contain attributes and
        /// further nested blocks.
        Block,
        /// Marks a struct as a managed resource. An optional positional name
        /// overrides the type name the runtime would otherwise infer from the
        /// struct identifier: `#[facet(terraform::resource("aws_s3_bucket"))]`,
        /// or bare `#[facet(terraform::resource)]` to infer `snake_case(Ident)`.
        Resource(Option<&'static str>),
        /// Marks a struct as a data source, with the same optional name override
        /// as [`Attr::Resource`].
        DataSource(Option<&'static str>),
        /// Marks a field as a data source lookup key. The [`SearchKey`] payload
        /// records the cardinality of a match:
        ///
        /// - `#[facet(terraform::search_key(exclusive))]` — the key is unique, so
        ///   a lookup yields at most one object (a singular data source).
        /// - `#[facet(terraform::search_key(shared))]` — the key is generic, so a
        ///   lookup may yield any number of objects (a plural data source whose
        ///   result is a list).
        SearchKey(SearchKey),
    }

    /// Cardinality payload for [`Attr::SearchKey`]. Exactly one of `exclusive`
    /// or `shared` is expected; `terraform-reflect` rejects neither/both.
    pub struct SearchKey {
        /// A unique key — a lookup returns at most one object.
        pub exclusive: bool,
        /// A generic key — a lookup may return many objects.
        pub shared: bool,
    }
}
