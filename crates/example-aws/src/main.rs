//! A minimal example Terraform/OpenTofu provider built with the SDK.
//!
//! It declares a single resource by reflecting a plain Rust struct and serves
//! its full create/read/update/delete lifecycle over the plugin protocol. The
//! resource has no external backing — it derives its one computed attribute
//! (`arn`) from configuration — which keeps the example self-contained and
//! deterministic.

use std::collections::HashMap;

use facet::Facet;
use terraform_provider::terraform;
use terraform_runtime::{async_trait, serve, Provider, Resource, ResourceError};

/// An S3-bucket-like resource.
#[derive(Facet)]
#[facet(terraform::resource)]
#[allow(dead_code)]
struct Bucket {
    /// The globally-unique name of the bucket.
    #[facet(terraform::required)]
    #[facet(terraform::force_new)]
    name: String,

    /// The ARN, derived from the name after creation.
    #[facet(terraform::computed)]
    arn: String,

    /// Free-form tags.
    tags: Option<HashMap<String, String>>,
}

impl Bucket {
    /// Derive the computed ARN from the (known) name.
    fn computed(mut self) -> Self {
        self.arn = format!("arn:aws:s3:::{}", self.name);
        self
    }
}

/// The handler for `aws_s3_bucket`.
struct BucketResource;

#[async_trait]
impl Resource for BucketResource {
    type Model = Bucket;

    async fn create(&self, planned: Bucket) -> Result<Bucket, ResourceError> {
        Ok(planned.computed())
    }

    async fn update(&self, planned: Bucket, _prior: Bucket) -> Result<Bucket, ResourceError> {
        Ok(planned.computed())
    }
}

#[tokio::main]
async fn main() {
    let provider = Provider::builder()
        .resource("aws_s3_bucket", BucketResource)
        .build()
        .expect("provider definition is valid");

    if let Err(err) = serve(provider).await {
        eprintln!("example-aws: failed to serve: {err}");
        std::process::exit(1);
    }
}
