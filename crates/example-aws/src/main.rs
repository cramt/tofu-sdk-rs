//! A minimal example Terraform/OpenTofu provider built with the SDK.
//!
//! It declares a single resource by reflecting a plain Rust struct, then serves
//! it over the plugin protocol. This is the whole author-facing surface for
//! Phase 2 (schema discovery); CRUD and planning land in later phases.

use facet::Facet;
use terraform_provider::terraform;
use terraform_runtime::{serve, Provider};

/// An S3-bucket-like resource.
#[derive(Facet)]
#[facet(terraform::resource)]
#[allow(dead_code)]
struct Bucket {
    /// The globally-unique name of the bucket.
    #[facet(terraform::required)]
    #[facet(terraform::force_new)]
    name: String,

    /// The ARN assigned after creation.
    #[facet(terraform::computed)]
    arn: String,

    /// Free-form tags.
    tags: std::collections::HashMap<String, String>,
}

#[tokio::main]
async fn main() {
    let provider = Provider::builder()
        .resource::<Bucket>("aws_s3_bucket")
        .build()
        .expect("provider definition is valid");

    if let Err(err) = serve(provider).await {
        eprintln!("example-aws: failed to serve: {err}");
        std::process::exit(1);
    }
}
