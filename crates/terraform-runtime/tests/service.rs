//! Tests for the gRPC service logic, calling the trait methods directly.
//!
//! The generated [`Provider`] service is an ordinary async trait, so there is no
//! need for a transport, socket, or client here — we construct the service and
//! call its methods. Transport correctness is tonic's concern and is covered for
//! real by the `tofu providers schema` contract test in `example-aws`.

use std::collections::HashMap;

use facet::Facet;
use serde_json::{json, Value};
use terraform_attrs as terraform;
use terraform_runtime::{Provider, ProviderService};
use terraform_tfplugin6::tfplugin6::{self, provider_server::Provider as _};
use tonic::{Code, Request};

#[derive(Facet)]
#[facet(terraform::resource)]
#[allow(dead_code)]
struct Bucket {
    /// The name of the bucket.
    #[facet(terraform::required)]
    #[facet(terraform::force_new)]
    name: String,
    #[facet(terraform::computed)]
    arn: String,
    tags: HashMap<String, String>,
}

#[derive(Facet)]
#[facet(terraform::data_source)]
#[allow(dead_code)]
struct BucketLookup {
    #[facet(terraform::required)]
    name: String,
    #[facet(terraform::computed)]
    arn: String,
}

fn service() -> ProviderService {
    let provider = Provider::builder()
        .resource::<Bucket>("aws_s3_bucket")
        .data_source::<BucketLookup>("aws_s3_bucket")
        .build()
        .expect("provider builds");
    ProviderService::new(provider)
}

#[tokio::test]
async fn get_provider_schema_returns_reflected_schema() {
    let svc = service();
    let resp = svc
        .get_provider_schema(Request::new(tfplugin6::get_provider_schema::Request {}))
        .await
        .expect("GetProviderSchema")
        .into_inner();

    let block = resp
        .resource_schemas
        .get("aws_s3_bucket")
        .expect("resource present")
        .block
        .as_ref()
        .expect("block present");

    let name = block.attributes.iter().find(|a| a.name == "name").unwrap();
    assert!(name.required);
    assert_eq!(cty(&name.r#type), json!("string"));

    let tags = block.attributes.iter().find(|a| a.name == "tags").unwrap();
    assert_eq!(cty(&tags.r#type), json!(["map", "string"]));

    assert!(resp.data_source_schemas.contains_key("aws_s3_bucket"));
    assert!(resp.server_capabilities.is_some());
}

#[tokio::test]
async fn get_metadata_lists_type_names() {
    let svc = service();
    let resp = svc
        .get_metadata(Request::new(tfplugin6::get_metadata::Request {}))
        .await
        .expect("GetMetadata")
        .into_inner();

    let resources: Vec<&str> = resp
        .resources
        .iter()
        .map(|r| r.type_name.as_str())
        .collect();
    assert_eq!(resources, vec!["aws_s3_bucket"]);
    let data_sources: Vec<&str> = resp
        .data_sources
        .iter()
        .map(|d| d.type_name.as_str())
        .collect();
    assert_eq!(data_sources, vec!["aws_s3_bucket"]);
}

#[tokio::test]
async fn unimplemented_rpc_returns_unimplemented() {
    let svc = service();
    let status = svc
        .configure_provider(Request::new(
            tfplugin6::configure_provider::Request::default(),
        ))
        .await
        .expect_err("configure_provider is not implemented yet");
    assert_eq!(status.code(), Code::Unimplemented);
}

/// Decode the `cty` JSON type-constraint bytes from an attribute's `type` field.
fn cty(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).expect("attribute type is valid JSON")
}
