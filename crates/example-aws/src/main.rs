//! A minimal example Terraform/OpenTofu provider built with the SDK.
//!
//! It declares a single resource by reflecting a plain Rust struct and serves
//! its full create/read/update/delete lifecycle over the plugin protocol. The
//! resource has no external backing — it derives its computed attributes
//! (`arn`, `region`) from configuration and the configured provider state —
//! which keeps the example self-contained and deterministic.
//!
//! It also demonstrates **provider configuration**: the provider takes an
//! optional `region`, and `configure` turns that into a shared `AwsClient`
//! (the *meta*) handed to the resource handler, which stamps the region onto
//! every bucket it creates.

use std::collections::HashMap;
use std::sync::Arc;

use facet::Facet;
use terraform_provider::terraform;
use terraform_runtime::{
    async_trait, serve, DataSource, DataSourceError, Provider, Resource, ResourceError,
};

/// Provider-level configuration.
#[derive(Facet)]
#[allow(dead_code)]
struct AwsConfig {
    /// The region buckets are created in. Defaults to `us-east-1` when unset.
    #[facet(terraform::optional)]
    region: Option<String>,
}

/// The configured provider state shared by every resource handler. In a real
/// provider this would hold an API client, credentials, an HTTP pool, etc.
struct AwsClient {
    region: String,
}

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

    /// The region the bucket lives in, taken from provider configuration.
    #[facet(terraform::computed)]
    region: String,

    /// Which handler last wrote this resource: `"created"` or `"updated"`. It
    /// lets the `tofu test` suite observe whether a change replaced the bucket
    /// (the create path runs again) or updated it in place.
    #[facet(terraform::computed)]
    last_action: String,

    /// Free-form tags.
    tags: Option<HashMap<String, String>>,
}

/// The handler for `aws_s3_bucket`, holding the configured client.
struct BucketResource {
    client: Arc<AwsClient>,
}

impl BucketResource {
    /// Fill the computed attributes from the (known) name and configured region,
    /// recording which lifecycle handler ran in `last_action`.
    fn computed(&self, mut bucket: Bucket, action: &str) -> Bucket {
        bucket.arn = format!("arn:aws:s3:::{}", bucket.name);
        bucket.region = self.client.region.clone();
        bucket.last_action = action.to_string();
        bucket
    }
}

#[async_trait]
impl Resource for BucketResource {
    type Model = Bucket;

    async fn create(&self, planned: Bucket) -> Result<Bucket, ResourceError> {
        Ok(self.computed(planned, "created"))
    }

    async fn update(&self, planned: Bucket, _prior: Bucket) -> Result<Bucket, ResourceError> {
        Ok(self.computed(planned, "updated"))
    }
}

/// A read-only lookup of a bucket's derived attributes by name. It mirrors the
/// resource's computed attributes but is queried with `data "aws_s3_bucket"`,
/// demonstrating a meta-backed data source (it reads the configured region from
/// the same shared `AwsClient`).
#[derive(Facet)]
#[facet(terraform::data_source)]
#[allow(dead_code)]
struct BucketLookup {
    /// The name of the bucket to look up.
    #[facet(terraform::required)]
    name: String,

    /// The ARN, derived from the name.
    #[facet(terraform::computed)]
    arn: String,

    /// The region, taken from provider configuration.
    #[facet(terraform::computed)]
    region: String,
}

/// The handler for the `aws_s3_bucket` data source, holding the configured client.
struct BucketDataSource {
    client: Arc<AwsClient>,
}

#[async_trait]
impl DataSource for BucketDataSource {
    type Model = BucketLookup;

    async fn read(&self, mut config: BucketLookup) -> Result<BucketLookup, DataSourceError> {
        config.arn = format!("arn:aws:s3:::{}", config.name);
        config.region = self.client.region.clone();
        Ok(config)
    }
}

#[tokio::main]
async fn main() {
    let provider = Provider::builder()
        .provider_config::<AwsConfig>()
        .configure(|cfg: AwsConfig| async move {
            Arc::new(AwsClient {
                region: cfg.region.unwrap_or_else(|| "us-east-1".to_string()),
            })
        })
        .resource_with("aws_s3_bucket", |client: Arc<AwsClient>| BucketResource {
            client,
        })
        .data_source_with("aws_s3_bucket", |client: Arc<AwsClient>| BucketDataSource {
            client,
        })
        .build()
        .expect("provider definition is valid");

    if let Err(err) = serve(provider).await {
        eprintln!("example-aws: failed to serve: {err}");
        std::process::exit(1);
    }
}
