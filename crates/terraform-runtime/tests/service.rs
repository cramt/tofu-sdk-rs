//! Tests for the gRPC service logic, calling the trait methods directly.
//!
//! The generated [`Provider`] service is an ordinary async trait, so there is no
//! need for a transport, socket, or client here — we construct the service and
//! call its methods. Transport correctness is tonic's concern and is covered for
//! real by the `tofu providers schema` contract test in `example-aws`.

use std::collections::HashMap;
use std::sync::Arc;

use facet::Facet;
use terraform_attrs as terraform;
use terraform_runtime::{
    async_trait, DataSource, DataSourceError, DataSourceList, Provider, ProviderService, Resource,
    ResourceError,
};
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

struct BucketResource;

#[async_trait]
impl Resource for BucketResource {
    type Model = Bucket;

    async fn create(&self, planned: Bucket) -> Result<Bucket, ResourceError> {
        Ok(planned)
    }
}

#[derive(Facet)]
#[facet(terraform::data_source)]
#[allow(dead_code)]
struct BucketLookup {
    #[facet(terraform::search_key(exclusive))]
    name: String,
    #[facet(terraform::computed)]
    arn: String,
}

struct BucketLookupDataSource;

#[async_trait]
impl DataSource for BucketLookupDataSource {
    type Model = BucketLookup;

    async fn read(&self, mut config: BucketLookup) -> Result<BucketLookup, DataSourceError> {
        config.arn = format!("arn:aws:s3:::{}", config.name);
        Ok(config)
    }
}

/// A plural data source model: looked up by a generic (`shared`) `cluster` key.
#[derive(Facet)]
#[facet(terraform::data_source)]
#[allow(dead_code)]
struct ServerLookup {
    #[facet(terraform::search_key(shared))]
    cluster: String,
    #[facet(terraform::computed)]
    id: String,
}

struct ServersByCluster;

#[async_trait]
impl DataSourceList for ServersByCluster {
    type Model = ServerLookup;

    async fn list(&self, query: ServerLookup) -> Result<Vec<ServerLookup>, DataSourceError> {
        Ok((0..2)
            .map(|i| ServerLookup {
                id: format!("{}-{i}", query.cluster),
                cluster: query.cluster.clone(),
            })
            .collect())
    }
}

fn service() -> ProviderService {
    let provider = Provider::builder()
        .resource("aws_s3_bucket", BucketResource)
        .data_source("aws_s3_bucket", BucketLookupDataSource)
        .data_source_list("servers", ServersByCluster)
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
    assert_eq!(cty(&name.r#type), r#""string""#);

    let tags = block.attributes.iter().find(|a| a.name == "tags").unwrap();
    assert_eq!(cty(&tags.r#type), r#"["map","string"]"#);

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
    let mut data_sources: Vec<&str> = resp
        .data_sources
        .iter()
        .map(|d| d.type_name.as_str())
        .collect();
    data_sources.sort_unstable();
    assert_eq!(data_sources, vec!["aws_s3_bucket", "servers"]);
}

#[tokio::test]
async fn unimplemented_rpc_returns_unimplemented() {
    let svc = service();
    // Import is not implemented yet.
    let status = svc
        .import_resource_state(Request::new(
            tfplugin6::import_resource_state::Request::default(),
        ))
        .await
        .expect_err("import_resource_state is not implemented yet");
    assert_eq!(status.code(), Code::Unimplemented);
}

#[tokio::test]
async fn read_data_source_computes_state() {
    let svc = service();

    // The data source's cty object type: required `name`, computed `arn`.
    let cty_ty = terraform_value::Type::Object(vec![
        terraform_value::ObjectAttr {
            name: "name".into(),
            ty: terraform_value::Type::String,
            optional: false,
        },
        terraform_value::ObjectAttr {
            name: "arn".into(),
            ty: terraform_value::Type::String,
            optional: true,
        },
    ]);

    // Config: name known, computed arn null (as Terraform sends it).
    let mut obj = std::collections::BTreeMap::new();
    obj.insert(
        "name".to_string(),
        terraform_value::Value::String("lookup-me".into()),
    );
    obj.insert("arn".to_string(), terraform_value::Value::Null);
    let config = terraform_value::Value::Object(obj);
    let config_dv = tfplugin6::DynamicValue {
        msgpack: terraform_codec::encode_msgpack(&config, &cty_ty).unwrap(),
        json: vec![],
    };

    let resp = svc
        .read_data_source(Request::new(tfplugin6::read_data_source::Request {
            type_name: "aws_s3_bucket".into(),
            config: Some(config_dv),
            ..Default::default()
        }))
        .await
        .expect("read_data_source")
        .into_inner();
    assert!(resp.diagnostics.is_empty(), "{:?}", resp.diagnostics);

    let state = resp.state.expect("data source state");
    let value = terraform_codec::decode_msgpack(&state.msgpack, &cty_ty).unwrap();
    if let terraform_value::Value::Object(fields) = value {
        assert_eq!(
            fields["arn"],
            terraform_value::Value::String("arn:aws:s3:::lookup-me".into()),
            "the data source computed arn from the queried name"
        );
    } else {
        panic!("data source state should be an object");
    }
}

#[tokio::test]
async fn read_data_source_list_wraps_results() {
    use terraform_value::{ObjectAttr, Type, Value};

    let svc = service();

    // The plural data source's wrapper cty: a `cluster` input plus a computed
    // `results` list of `{ cluster, id }` objects.
    let element = Type::Object(vec![
        ObjectAttr {
            name: "cluster".into(),
            ty: Type::String,
            optional: true,
        },
        ObjectAttr {
            name: "id".into(),
            ty: Type::String,
            optional: true,
        },
    ]);
    let cty = Type::Object(vec![
        ObjectAttr {
            name: "cluster".into(),
            ty: Type::String,
            optional: true,
        },
        ObjectAttr {
            name: "results".into(),
            ty: Type::list(element),
            optional: true,
        },
    ]);

    // Config: the shared `cluster` key set, `results` not yet known.
    let mut obj = std::collections::BTreeMap::new();
    obj.insert("cluster".to_string(), Value::String("prod".into()));
    obj.insert("results".to_string(), Value::Null);
    let config_dv = tfplugin6::DynamicValue {
        msgpack: terraform_codec::encode_msgpack(&Value::Object(obj), &cty).unwrap(),
        json: vec![],
    };

    let resp = svc
        .read_data_source(Request::new(tfplugin6::read_data_source::Request {
            type_name: "servers".into(),
            config: Some(config_dv),
            ..Default::default()
        }))
        .await
        .expect("read_data_source")
        .into_inner();
    assert!(resp.diagnostics.is_empty(), "{:?}", resp.diagnostics);

    let state = resp.state.expect("data source state");
    let value = terraform_codec::decode_msgpack(&state.msgpack, &cty).unwrap();
    let Value::Object(fields) = value else {
        panic!("plural state should be an object");
    };
    let Value::List(results) = &fields["results"] else {
        panic!("results should be a list");
    };
    assert_eq!(results.len(), 2, "list handler returned two matches");
    let Value::Object(first) = &results[0] else {
        panic!("result element should be an object");
    };
    assert_eq!(first["id"], Value::String("prod-0".into()));
    assert_eq!(first["cluster"], Value::String("prod".into()));
}

#[tokio::test]
async fn read_data_source_unknown_type_errors() {
    let svc = service();
    let resp = svc
        .read_data_source(Request::new(tfplugin6::read_data_source::Request {
            type_name: "nonexistent".into(),
            ..Default::default()
        }))
        .await
        .expect("read_data_source")
        .into_inner();
    assert!(
        !resp.diagnostics.is_empty(),
        "unknown data source type should produce a diagnostic"
    );
}

#[tokio::test]
async fn configure_provider_accepts() {
    let svc = service();
    let resp = svc
        .configure_provider(Request::new(
            tfplugin6::configure_provider::Request::default(),
        ))
        .await
        .expect("configure succeeds")
        .into_inner();
    assert!(resp.diagnostics.is_empty());
}

#[tokio::test]
async fn plan_then_apply_create_round_trips() {
    let svc = service();

    // Build a proposed new state: name set, computed arn null (as Terraform sends).
    let cty_ty = terraform_value::Type::Object(vec![
        terraform_value::ObjectAttr {
            name: "name".into(),
            ty: terraform_value::Type::String,
            optional: false,
        },
        terraform_value::ObjectAttr {
            name: "arn".into(),
            ty: terraform_value::Type::String,
            optional: true,
        },
        terraform_value::ObjectAttr {
            name: "tags".into(),
            ty: terraform_value::Type::map(terraform_value::Type::String),
            optional: true,
        },
    ]);
    let mut obj = std::collections::BTreeMap::new();
    obj.insert(
        "name".to_string(),
        terraform_value::Value::String("b1".into()),
    );
    obj.insert("arn".to_string(), terraform_value::Value::Null);
    obj.insert(
        "tags".to_string(),
        terraform_value::Value::Map(Default::default()),
    );
    let proposed = terraform_value::Value::Object(obj);
    let proposed_dv = tfplugin6::DynamicValue {
        msgpack: terraform_codec::encode_msgpack(&proposed, &cty_ty).unwrap(),
        json: vec![],
    };

    // Plan: computed arn should become unknown.
    let plan = svc
        .plan_resource_change(Request::new(tfplugin6::plan_resource_change::Request {
            type_name: "aws_s3_bucket".into(),
            proposed_new_state: Some(proposed_dv.clone()),
            ..Default::default()
        }))
        .await
        .expect("plan")
        .into_inner();
    assert!(plan.diagnostics.is_empty());
    let planned_state = plan.planned_state.expect("planned state");
    let planned_value = terraform_codec::decode_msgpack(&planned_state.msgpack, &cty_ty).unwrap();
    if let terraform_value::Value::Object(fields) = &planned_value {
        assert!(
            fields["arn"].is_unknown(),
            "computed arn planned as unknown"
        );
    } else {
        panic!("planned state should be an object");
    }

    // Apply (create): prior null, planned from the plan above.
    let apply = svc
        .apply_resource_change(Request::new(tfplugin6::apply_resource_change::Request {
            type_name: "aws_s3_bucket".into(),
            prior_state: None,
            planned_state: Some(planned_state),
            ..Default::default()
        }))
        .await
        .expect("apply")
        .into_inner();
    assert!(apply.diagnostics.is_empty(), "{:?}", apply.diagnostics);
    let new_state = apply.new_state.expect("new state");
    let new_value = terraform_codec::decode_msgpack(&new_state.msgpack, &cty_ty).unwrap();
    if let terraform_value::Value::Object(fields) = new_value {
        assert_eq!(fields["name"], terraform_value::Value::String("b1".into()));
        // The handler echoed planned; arn was unknown -> decoded as "" -> echoed.
        // (A real handler would compute it; this confirms the round trip works.)
        assert!(matches!(fields["arn"], terraform_value::Value::String(_)));
    } else {
        panic!("new state should be an object");
    }
}

// --- Provider configuration (meta) -----------------------------------------

/// Provider config: an optional region.
#[derive(Facet)]
#[allow(dead_code)]
struct AwsConfig {
    #[facet(terraform::optional)]
    region: Option<String>,
}

/// The configured shared state handed to resource handlers.
struct AwsClient {
    region: String,
}

/// A resource that stamps the configured region onto itself.
#[derive(Facet)]
#[facet(terraform::resource)]
#[allow(dead_code)]
struct RegionBucket {
    #[facet(terraform::required)]
    name: String,
    #[facet(terraform::computed)]
    region: String,
}

struct RegionResource {
    client: Arc<AwsClient>,
}

#[async_trait]
impl Resource for RegionResource {
    type Model = RegionBucket;

    async fn create(&self, mut planned: RegionBucket) -> Result<RegionBucket, ResourceError> {
        planned.region = self.client.region.clone();
        Ok(planned)
    }
}

/// A provider whose `region_bucket` handler is built from the configured client.
fn configured_service() -> ProviderService {
    let provider = Provider::builder()
        .provider_config::<AwsConfig>()
        .configure(|cfg: AwsConfig| async move {
            Arc::new(AwsClient {
                region: cfg.region.unwrap_or_else(|| "us-east-1".to_string()),
            })
        })
        .resource_with("region_bucket", |client: Arc<AwsClient>| RegionResource {
            client,
        })
        .build()
        .expect("configured provider builds");
    ProviderService::new(provider)
}

/// The `cty` object type of `RegionBucket`.
fn region_bucket_ty() -> terraform_value::Type {
    terraform_value::Type::Object(vec![
        terraform_value::ObjectAttr {
            name: "name".into(),
            ty: terraform_value::Type::String,
            optional: false,
        },
        terraform_value::ObjectAttr {
            name: "region".into(),
            ty: terraform_value::Type::String,
            optional: true,
        },
    ])
}

#[tokio::test]
async fn configure_then_apply_uses_provider_meta() {
    let svc = configured_service();

    // ConfigureProvider with region = eu-west-1.
    let cfg_ty = terraform_value::Type::Object(vec![terraform_value::ObjectAttr {
        name: "region".into(),
        ty: terraform_value::Type::String,
        optional: true,
    }]);
    let mut cfg = std::collections::BTreeMap::new();
    cfg.insert(
        "region".to_string(),
        terraform_value::Value::String("eu-west-1".into()),
    );
    let cfg_dv = tfplugin6::DynamicValue {
        msgpack: terraform_codec::encode_msgpack(&terraform_value::Value::Object(cfg), &cfg_ty)
            .unwrap(),
        json: vec![],
    };
    let configured = svc
        .configure_provider(Request::new(tfplugin6::configure_provider::Request {
            config: Some(cfg_dv),
            ..Default::default()
        }))
        .await
        .expect("configure")
        .into_inner();
    assert!(
        configured.diagnostics.is_empty(),
        "{:?}",
        configured.diagnostics
    );

    // Apply (create): the handler should stamp the configured region.
    let ty = region_bucket_ty();
    let mut planned = std::collections::BTreeMap::new();
    planned.insert(
        "name".to_string(),
        terraform_value::Value::String("b".into()),
    );
    planned.insert("region".to_string(), terraform_value::Value::Unknown);
    let planned_dv = tfplugin6::DynamicValue {
        msgpack: terraform_codec::encode_msgpack(&terraform_value::Value::Object(planned), &ty)
            .unwrap(),
        json: vec![],
    };
    let apply = svc
        .apply_resource_change(Request::new(tfplugin6::apply_resource_change::Request {
            type_name: "region_bucket".into(),
            prior_state: None,
            planned_state: Some(planned_dv),
            ..Default::default()
        }))
        .await
        .expect("apply")
        .into_inner();
    assert!(apply.diagnostics.is_empty(), "{:?}", apply.diagnostics);

    let new_value =
        terraform_codec::decode_msgpack(&apply.new_state.expect("new state").msgpack, &ty).unwrap();
    let terraform_value::Value::Object(fields) = new_value else {
        panic!("new state should be an object");
    };
    assert_eq!(
        fields["region"],
        terraform_value::Value::String("eu-west-1".into()),
        "handler stamped the region from the configured meta"
    );
}

/// The `cty` JSON type-constraint bytes from an attribute's `type` field, as a
/// JSON string.
fn cty(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("attribute type is valid UTF-8 JSON")
}

// --- Dynamic seam (the FFI boundary for non-Rust frontends) -----------------

/// A resource handler written *without* facet or a `Model` — it operates on the
/// dynamic `Value` directly, exactly as the Node binding's bridge will.
struct DynEchoArn;

#[async_trait]
impl terraform_runtime::DynResource for DynEchoArn {
    async fn create(
        &self,
        planned: terraform_value::Value,
    ) -> Result<terraform_value::Value, terraform_runtime::Diagnostics> {
        let terraform_value::Value::Object(mut fields) = planned else {
            return Err(vec![terraform_runtime::Diag::error(
                "bad input",
                "expected an object",
            )]);
        };
        let name = match fields.get("name") {
            Some(terraform_value::Value::String(s)) => s.clone(),
            _ => String::new(),
        };
        fields.insert(
            "arn".to_string(),
            terraform_value::Value::String(format!("arn:aws:s3:::{name}")),
        );
        Ok(terraform_value::Value::Object(fields))
    }

    async fn read(
        &self,
        current: terraform_value::Value,
    ) -> Result<Option<terraform_value::Value>, terraform_runtime::Diagnostics> {
        Ok(Some(current))
    }

    async fn update(
        &self,
        planned: terraform_value::Value,
        _prior: terraform_value::Value,
    ) -> Result<terraform_value::Value, terraform_runtime::Diagnostics> {
        Ok(planned)
    }

    async fn delete(
        &self,
        _prior: terraform_value::Value,
    ) -> Result<(), terraform_runtime::Diagnostics> {
        Ok(())
    }
}

#[tokio::test]
async fn dyn_resource_serves_from_hand_built_schema() {
    use terraform_ir::{AttributeSchema, Block};
    use terraform_value::{ObjectAttr, Type, Value};

    // The IR a non-Rust frontend would construct from its own schema description.
    let block = Block {
        attributes: vec![
            AttributeSchema {
                required: true,
                ..AttributeSchema::new("name", Type::String)
            },
            AttributeSchema {
                computed: true,
                ..AttributeSchema::new("arn", Type::String)
            },
        ],
        nested_blocks: vec![],
    };
    let provider = Provider::builder()
        .dyn_resource("aws_s3_bucket", block, std::sync::Arc::new(DynEchoArn))
        .build()
        .expect("dynamic provider builds");
    let svc = ProviderService::new(provider);

    // The hand-built schema is served like any other.
    let schema = svc
        .get_provider_schema(Request::new(tfplugin6::get_provider_schema::Request {}))
        .await
        .expect("GetProviderSchema")
        .into_inner();
    assert!(schema.resource_schemas.contains_key("aws_s3_bucket"));

    // A create round-trips through the dynamic handler.
    let cty = Type::Object(vec![
        ObjectAttr {
            name: "name".into(),
            ty: Type::String,
            optional: false,
        },
        ObjectAttr {
            name: "arn".into(),
            ty: Type::String,
            optional: true,
        },
    ]);
    let mut obj = std::collections::BTreeMap::new();
    obj.insert("name".to_string(), Value::String("b1".into()));
    obj.insert("arn".to_string(), Value::Unknown);
    let planned = tfplugin6::DynamicValue {
        msgpack: terraform_codec::encode_msgpack(&Value::Object(obj), &cty).unwrap(),
        json: vec![],
    };

    let apply = svc
        .apply_resource_change(Request::new(tfplugin6::apply_resource_change::Request {
            type_name: "aws_s3_bucket".into(),
            prior_state: None,
            planned_state: Some(planned),
            ..Default::default()
        }))
        .await
        .expect("apply")
        .into_inner();
    assert!(apply.diagnostics.is_empty(), "{:?}", apply.diagnostics);

    let new = terraform_codec::decode_msgpack(&apply.new_state.unwrap().msgpack, &cty).unwrap();
    let Value::Object(fields) = new else {
        panic!("new state should be an object");
    };
    assert_eq!(
        fields["arn"],
        Value::String("arn:aws:s3:::b1".into()),
        "the dynamic handler computed the arn"
    );
}
