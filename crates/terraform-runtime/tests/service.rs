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
    async_trait, ConfigureError, Ctx, DataSource, DataSourceError, DataSourceList, Diag, Ephemeral,
    EphemeralError, EphemeralFromResource, Path, PlanModifications, Provider, ProviderService,
    Resource, ResourceError,
};
use terraform_tfplugin6::tfplugin6::{self, provider_server::Provider as _};
use tonic::{Code, Request};

#[derive(Facet)]
#[facet(terraform::resource("aws_s3_bucket"))]
#[allow(dead_code)]
struct Bucket {
    /// The name of the bucket.
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

    async fn create(&self, _ctx: &mut Ctx, planned: Bucket) -> Result<Bucket, ResourceError> {
        Ok(planned)
    }
}

#[derive(Facet)]
#[facet(terraform::data_source("aws_s3_bucket"))]
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

    async fn read(
        &self,
        _ctx: &mut Ctx,
        mut config: BucketLookup,
    ) -> Result<BucketLookup, DataSourceError> {
        config.arn = format!("arn:aws:s3:::{}", config.name);
        Ok(config)
    }
}

/// A plural data source model: looked up by a generic (`shared`) `cluster` key.
/// The plural builder appends `s`, so `data_source("server")` registers as
/// `servers`.
#[derive(Facet)]
#[facet(terraform::data_source("server"))]
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

    async fn list(
        &self,
        _ctx: &mut Ctx,
        query: ServerLookup,
    ) -> Result<Vec<ServerLookup>, DataSourceError> {
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
        .resource(BucketResource)
        .data_source(BucketLookupDataSource)
        .data_source_list(ServersByCluster)
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
    // Resource identity is not implemented yet (a still-stubbed RPC).
    let status = svc
        .get_resource_identity_schemas(Request::new(
            tfplugin6::get_resource_identity_schemas::Request::default(),
        ))
        .await
        .expect_err("get_resource_identity_schemas is not implemented yet");
    assert_eq!(status.code(), Code::Unimplemented);
}

/// A resource that forces replacement when its (non-force_new) `tier` changes,
/// via `modify_plan` — proving the author plan hook runs and feeds back into the
/// mechanical plan.
#[derive(Facet)]
#[facet(terraform::resource("planmod"))]
#[allow(dead_code)]
struct PlanMod {
    name: String,
    #[facet(terraform::optional)]
    tier: Option<String>,
}

struct PlanModResource;

#[async_trait]
impl Resource for PlanModResource {
    type Model = PlanMod;

    async fn create(&self, _ctx: &mut Ctx, planned: PlanMod) -> Result<PlanMod, ResourceError> {
        Ok(planned)
    }

    async fn modify_plan(
        &self,
        _ctx: &mut Ctx,
        prior: Option<PlanMod>,
        proposed: PlanMod,
    ) -> Result<PlanModifications, ResourceError> {
        let mods = PlanModifications::new();
        match prior {
            Some(prior) if prior.tier != proposed.tier => Ok(mods.require_replace("tier")),
            _ => Ok(mods),
        }
    }
}

#[tokio::test]
async fn modify_plan_can_force_replacement() {
    let svc = Provider::builder()
        .resource(PlanModResource)
        .build()
        .map(ProviderService::new)
        .expect("provider builds");

    let ty = terraform_value::Type::Object(vec![
        terraform_value::ObjectAttr {
            name: "name".into(),
            ty: terraform_value::Type::String,
            optional: false,
        },
        terraform_value::ObjectAttr {
            name: "tier".into(),
            ty: terraform_value::Type::String,
            optional: true,
        },
    ]);
    let state = |name: &str, tier: &str| {
        let mut obj = std::collections::BTreeMap::new();
        obj.insert(
            "name".to_string(),
            terraform_value::Value::String(name.into()),
        );
        obj.insert(
            "tier".to_string(),
            terraform_value::Value::String(tier.into()),
        );
        tfplugin6::DynamicValue {
            msgpack: terraform_codec::encode_msgpack(&terraform_value::Value::Object(obj), &ty)
                .unwrap(),
            json: vec![],
        }
    };

    // tier changes silver -> gold: modify_plan forces replacement on `tier`.
    let plan = svc
        .plan_resource_change(Request::new(tfplugin6::plan_resource_change::Request {
            type_name: "planmod".into(),
            prior_state: Some(state("a", "silver")),
            proposed_new_state: Some(state("a", "gold")),
            ..Default::default()
        }))
        .await
        .expect("plan")
        .into_inner();
    assert!(plan.diagnostics.is_empty(), "{:?}", plan.diagnostics);
    assert_eq!(plan.requires_replace.len(), 1, "tier change forces replace");
    match plan.requires_replace[0].steps[0].selector.as_ref().unwrap() {
        tfplugin6::attribute_path::step::Selector::AttributeName(n) => assert_eq!(n, "tier"),
        other => panic!("expected attribute-name step, got {other:?}"),
    }

    // tier unchanged: no replacement.
    let plan = svc
        .plan_resource_change(Request::new(tfplugin6::plan_resource_change::Request {
            type_name: "planmod".into(),
            prior_state: Some(state("a", "gold")),
            proposed_new_state: Some(state("a", "gold")),
            ..Default::default()
        }))
        .await
        .expect("plan")
        .into_inner();
    assert!(
        plan.requires_replace.is_empty(),
        "no tier change, no replace: {:?}",
        plan.requires_replace
    );
}

/// A resource with a write-only `password` input. `create` records the value it
/// received so the test can prove the handler saw the real (apply-time config)
/// secret, while the returned state must null it out.
#[derive(Facet)]
#[facet(terraform::resource("wo_secret"))]
#[allow(dead_code)]
struct WoSecret {
    name: String,
    #[facet(terraform::write_only)]
    password: Option<String>,
}

struct WoResource {
    seen: Arc<std::sync::Mutex<Option<String>>>,
}

#[async_trait]
impl Resource for WoResource {
    type Model = WoSecret;

    async fn create(&self, _ctx: &mut Ctx, planned: WoSecret) -> Result<WoSecret, ResourceError> {
        *self.seen.lock().unwrap() = planned.password.clone();
        Ok(planned)
    }
}

#[tokio::test]
async fn write_only_value_reaches_handler_but_not_state() {
    let seen = Arc::new(std::sync::Mutex::new(None));
    let svc = Provider::builder()
        .resource(WoResource { seen: seen.clone() })
        .build()
        .map(ProviderService::new)
        .expect("provider builds");

    let ty = terraform_value::Type::Object(vec![
        terraform_value::ObjectAttr {
            name: "name".into(),
            ty: terraform_value::Type::String,
            optional: false,
        },
        terraform_value::ObjectAttr {
            name: "password".into(),
            ty: terraform_value::Type::String,
            optional: true,
        },
    ]);
    let state = |name: &str, password: terraform_value::Value| {
        let mut obj = std::collections::BTreeMap::new();
        obj.insert(
            "name".to_string(),
            terraform_value::Value::String(name.into()),
        );
        obj.insert("password".to_string(), password);
        tfplugin6::DynamicValue {
            msgpack: terraform_codec::encode_msgpack(&terraform_value::Value::Object(obj), &ty)
                .unwrap(),
            json: vec![],
        }
    };

    // Create: planned state nulls the write-only password; config carries it.
    let resp = svc
        .apply_resource_change(Request::new(tfplugin6::apply_resource_change::Request {
            type_name: "wo_secret".into(),
            prior_state: None,
            planned_state: Some(state("db", terraform_value::Value::Null)),
            config: Some(state(
                "db",
                terraform_value::Value::String("hunter2".into()),
            )),
            ..Default::default()
        }))
        .await
        .expect("apply")
        .into_inner();
    assert!(resp.diagnostics.is_empty(), "{:?}", resp.diagnostics);

    // The handler observed the real secret from config...
    assert_eq!(
        *seen.lock().unwrap(),
        Some("hunter2".to_string()),
        "create should receive the write-only value merged from config"
    );

    // ...but it must never be written to state.
    let new_state = resp.new_state.expect("new state");
    let new_state =
        terraform_codec::decode_msgpack(&new_state.msgpack, &ty).expect("decode new state");
    let terraform_value::Value::Object(fields) = new_state else {
        panic!("new state should be an object");
    };
    assert_eq!(fields["name"], terraform_value::Value::String("db".into()));
    assert!(
        fields["password"].is_null(),
        "write-only password must be null in persisted state, got {:?}",
        fields["password"]
    );
}

/// A resource with a `settings` list block carrying a computed `id`, whose
/// `modify_plan` marks the *nested* `settings[0].id` unknown by path — proving a
/// plan modification can reach inside a block, not just a top-level attribute,
/// and that it survives the `PlanResourceChange` encode round-trip.
#[derive(Facet)]
#[facet(terraform::resource("planmod_nested"))]
#[allow(dead_code)]
struct PlanModNested {
    name: String,
    #[facet(terraform::block)]
    settings: Vec<Setting>,
}

#[derive(Facet)]
#[allow(dead_code)]
struct Setting {
    key: String,
    #[facet(terraform::computed)]
    id: String,
}

struct PlanModNestedResource;

#[async_trait]
impl Resource for PlanModNestedResource {
    type Model = PlanModNested;

    async fn create(
        &self,
        _ctx: &mut Ctx,
        planned: PlanModNested,
    ) -> Result<PlanModNested, ResourceError> {
        Ok(planned)
    }

    async fn modify_plan(
        &self,
        _ctx: &mut Ctx,
        _prior: Option<PlanModNested>,
        _proposed: PlanModNested,
    ) -> Result<PlanModifications, ResourceError> {
        // Recompute the first setting's id by rule, addressing it by nested path.
        Ok(PlanModifications::new()
            .unknown(Path::root().attribute("settings").index(0).attribute("id")))
    }
}

#[tokio::test]
async fn modify_plan_marks_nested_block_attribute_unknown() {
    let svc = Provider::builder()
        .resource(PlanModNestedResource)
        .build()
        .map(ProviderService::new)
        .expect("provider builds");

    // The resource's cty type, with `settings` a list(object({key, id})).
    let setting_ty = terraform_value::Type::Object(vec![
        terraform_value::ObjectAttr {
            name: "key".into(),
            ty: terraform_value::Type::String,
            optional: false,
        },
        terraform_value::ObjectAttr {
            name: "id".into(),
            ty: terraform_value::Type::String,
            optional: false,
        },
    ]);
    let ty = terraform_value::Type::Object(vec![
        terraform_value::ObjectAttr {
            name: "name".into(),
            ty: terraform_value::Type::String,
            optional: false,
        },
        terraform_value::ObjectAttr {
            name: "settings".into(),
            ty: terraform_value::Type::list(setting_ty.clone()),
            optional: false,
        },
    ]);

    // A create (null prior) carrying a *known* settings[0].id; the rule should
    // overwrite it with unknown.
    let mut setting = std::collections::BTreeMap::new();
    setting.insert(
        "key".to_string(),
        terraform_value::Value::String("k".into()),
    );
    setting.insert(
        "id".to_string(),
        terraform_value::Value::String("known".into()),
    );
    let mut obj = std::collections::BTreeMap::new();
    obj.insert(
        "name".to_string(),
        terraform_value::Value::String("a".into()),
    );
    obj.insert(
        "settings".to_string(),
        terraform_value::Value::List(vec![terraform_value::Value::Object(setting)]),
    );
    let proposed = tfplugin6::DynamicValue {
        msgpack: terraform_codec::encode_msgpack(&terraform_value::Value::Object(obj), &ty)
            .unwrap(),
        json: vec![],
    };

    let plan = svc
        .plan_resource_change(Request::new(tfplugin6::plan_resource_change::Request {
            type_name: "planmod_nested".into(),
            prior_state: None,
            proposed_new_state: Some(proposed),
            ..Default::default()
        }))
        .await
        .expect("plan")
        .into_inner();
    assert!(plan.diagnostics.is_empty(), "{:?}", plan.diagnostics);

    // Decode the planned new state and assert settings[0].id came back unknown.
    let planned = plan.planned_state.expect("planned new state");
    let planned = terraform_codec::decode_msgpack(&planned.msgpack, &ty).expect("decode planned");
    let terraform_value::Value::Object(fields) = planned else {
        panic!("planned should be an object");
    };
    let terraform_value::Value::List(items) = &fields["settings"] else {
        panic!("settings should be a list");
    };
    let terraform_value::Value::Object(first) = &items[0] else {
        panic!("settings[0] should be an object");
    };
    assert!(
        first["id"].is_unknown(),
        "settings[0].id should be planned unknown by the nested path modification"
    );
    assert_eq!(first["key"], terraform_value::Value::String("k".into()));
}

/// A resource whose `create` records whether it observed cancellation, proving
/// the runtime exposes the `StopProvider` token to in-flight handlers.
#[derive(Facet)]
#[facet(terraform::resource("cancel_probe"))]
#[allow(dead_code)]
struct CancelProbe {
    name: String,
}

struct CancelProbeResource {
    observed: Arc<std::sync::Mutex<Option<bool>>>,
}

#[async_trait]
impl Resource for CancelProbeResource {
    type Model = CancelProbe;

    async fn create(
        &self,
        ctx: &mut Ctx,
        planned: CancelProbe,
    ) -> Result<CancelProbe, ResourceError> {
        // Observe cancellation through the handler ctx (it carries the same token
        // `current_cancellation()` exposes ambiently).
        *self.observed.lock().unwrap() = Some(ctx.is_cancelled());
        Ok(planned)
    }
}

#[tokio::test]
async fn stop_provider_acknowledges_and_handlers_observe_cancellation() {
    let observed = Arc::new(std::sync::Mutex::new(None));
    let svc = Provider::builder()
        .resource(CancelProbeResource {
            observed: Arc::clone(&observed),
        })
        .build()
        .map(ProviderService::new)
        .expect("provider builds");

    let ty = terraform_value::Type::Object(vec![terraform_value::ObjectAttr {
        name: "name".into(),
        ty: terraform_value::Type::String,
        optional: false,
    }]);
    let planned_dv = |name: &str| {
        let mut obj = std::collections::BTreeMap::new();
        obj.insert(
            "name".to_string(),
            terraform_value::Value::String(name.into()),
        );
        tfplugin6::DynamicValue {
            msgpack: terraform_codec::encode_msgpack(&terraform_value::Value::Object(obj), &ty)
                .unwrap(),
            json: vec![],
        }
    };
    let apply = |dv: tfplugin6::DynamicValue| {
        svc.apply_resource_change(Request::new(tfplugin6::apply_resource_change::Request {
            type_name: "cancel_probe".into(),
            prior_state: None,
            planned_state: Some(dv),
            ..Default::default()
        }))
    };

    // Before StopProvider, the handler sees a live (un-cancelled) token.
    apply(planned_dv("before")).await.expect("apply before");
    assert_eq!(
        *observed.lock().unwrap(),
        Some(false),
        "handler should see an un-cancelled token during normal dispatch"
    );

    // StopProvider acknowledges (no longer Unimplemented) with no error.
    let stop = svc
        .stop_provider(Request::new(tfplugin6::stop_provider::Request {}))
        .await
        .expect("stop_provider returns OK")
        .into_inner();
    assert!(stop.error.is_empty(), "stop error: {:?}", stop.error);

    // After StopProvider, an in-flight handler observes the tripped token.
    apply(planned_dv("after")).await.expect("apply after");
    assert_eq!(
        *observed.lock().unwrap(),
        Some(true),
        "handler should observe cancellation after StopProvider"
    );
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
#[facet(terraform::resource("region_bucket"))]
#[allow(dead_code)]
struct RegionBucket {
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

    async fn create(
        &self,
        _ctx: &mut Ctx,
        mut planned: RegionBucket,
    ) -> Result<RegionBucket, ResourceError> {
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
        .resource_with(|client: Arc<AwsClient>| RegionResource { client })
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

/// A provider whose `configure` closure rejects a bad region with a diagnostic.
fn fallible_service() -> ProviderService {
    Provider::builder()
        .provider_config::<AwsConfig>()
        .configure(|cfg: AwsConfig| async move {
            match cfg.region.as_deref() {
                Some("bad") => Err(ConfigureError::new("invalid region")
                    .with_detail("`bad` is not a valid region")),
                _ => Ok(Arc::new(AwsClient {
                    region: cfg.region.unwrap_or_else(|| "us-east-1".to_string()),
                })),
            }
        })
        .resource_with(|client: Arc<AwsClient>| RegionResource { client })
        .build()
        .map(ProviderService::new)
        .expect("fallible provider builds")
}

fn aws_config_dv(region: &str) -> tfplugin6::DynamicValue {
    let cfg_ty = terraform_value::Type::Object(vec![terraform_value::ObjectAttr {
        name: "region".into(),
        ty: terraform_value::Type::String,
        optional: true,
    }]);
    let mut cfg = std::collections::BTreeMap::new();
    cfg.insert(
        "region".to_string(),
        terraform_value::Value::String(region.into()),
    );
    tfplugin6::DynamicValue {
        msgpack: terraform_codec::encode_msgpack(&terraform_value::Value::Object(cfg), &cfg_ty)
            .unwrap(),
        json: vec![],
    }
}

#[tokio::test]
async fn configure_provider_rejects_bad_config_with_diagnostic() {
    let svc = fallible_service();
    let resp = svc
        .configure_provider(Request::new(tfplugin6::configure_provider::Request {
            config: Some(aws_config_dv("bad")),
            ..Default::default()
        }))
        .await
        .expect("configure call returns")
        .into_inner();
    assert_eq!(resp.diagnostics.len(), 1, "{:?}", resp.diagnostics);
    assert_eq!(resp.diagnostics[0].summary, "invalid region");
    assert_eq!(resp.diagnostics[0].detail, "`bad` is not a valid region");
}

#[tokio::test]
async fn configure_provider_accepts_good_config_when_fallible() {
    let svc = fallible_service();
    let resp = svc
        .configure_provider(Request::new(tfplugin6::configure_provider::Request {
            config: Some(aws_config_dv("eu-west-1")),
            ..Default::default()
        }))
        .await
        .expect("configure call returns")
        .into_inner();
    assert!(resp.diagnostics.is_empty(), "{:?}", resp.diagnostics);
}

#[tokio::test]
async fn validate_provider_config_surfaces_diagnostics() {
    let svc = Provider::builder()
        .provider_config::<AwsConfig>()
        .validate_config(|cfg: AwsConfig| async move {
            match cfg.region.as_deref() {
                Some("bad") => {
                    vec![Diag::error("invalid region", "`bad` is reserved").at(["region"])]
                }
                _ => Vec::new(),
            }
        })
        .build()
        .map(ProviderService::new)
        .expect("provider builds");

    // A bad region yields one diagnostic, pointed at the `region` attribute.
    let bad = svc
        .validate_provider_config(Request::new(tfplugin6::validate_provider_config::Request {
            config: Some(aws_config_dv("bad")),
        }))
        .await
        .expect("validate_provider_config")
        .into_inner();
    assert_eq!(bad.diagnostics.len(), 1, "{:?}", bad.diagnostics);
    assert_eq!(bad.diagnostics[0].summary, "invalid region");
    let path = bad.diagnostics[0]
        .attribute
        .as_ref()
        .expect("diagnostic carries an attribute path");
    match path.steps[0].selector.as_ref().unwrap() {
        tfplugin6::attribute_path::step::Selector::AttributeName(n) => assert_eq!(n, "region"),
        other => panic!("expected an attribute-name step, got {other:?}"),
    }

    // A good region validates clean.
    let ok = svc
        .validate_provider_config(Request::new(tfplugin6::validate_provider_config::Request {
            config: Some(aws_config_dv("eu-west-1")),
        }))
        .await
        .expect("validate_provider_config")
        .into_inner();
    assert!(ok.diagnostics.is_empty(), "{:?}", ok.diagnostics);
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

    async fn import(
        &self,
        id: String,
    ) -> Result<terraform_value::Value, terraform_runtime::Diagnostics> {
        // Import by name: produce a state with `name` set, `arn` computed.
        let mut fields = std::collections::BTreeMap::new();
        fields.insert(
            "name".to_string(),
            terraform_value::Value::String(id.clone()),
        );
        fields.insert(
            "arn".to_string(),
            terraform_value::Value::String(format!("arn:aws:s3:::{id}")),
        );
        Ok(terraform_value::Value::Object(fields))
    }

    async fn upgrade(
        &self,
        _from_version: i64,
        prior: terraform_value::Value,
    ) -> Result<terraform_value::Value, terraform_runtime::Diagnostics> {
        // Migrate by renaming the old `bucket` field to `name`, recomputing arn.
        let terraform_value::Value::Object(old) = prior else {
            return Err(vec![terraform_runtime::Diag::error(
                "bad prior state",
                "expected an object",
            )]);
        };
        let name = match old.get("bucket") {
            Some(terraform_value::Value::String(s)) => s.clone(),
            _ => String::new(),
        };
        let mut fields = std::collections::BTreeMap::new();
        fields.insert(
            "name".to_string(),
            terraform_value::Value::String(name.clone()),
        );
        fields.insert(
            "arn".to_string(),
            terraform_value::Value::String(format!("arn:aws:s3:::{name}")),
        );
        Ok(terraform_value::Value::Object(fields))
    }

    async fn validate(&self, config: terraform_value::Value) -> terraform_runtime::Diagnostics {
        // Reject a bucket named "bad", pointing at the `name` attribute.
        let name = match &config {
            terraform_value::Value::Object(o) => match o.get("name") {
                Some(terraform_value::Value::String(s)) => s.as_str(),
                _ => "",
            },
            _ => "",
        };
        if name == "bad" {
            vec![terraform_runtime::Diag::error("invalid name", "`bad` is reserved").at(["name"])]
        } else {
            Vec::new()
        }
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
        .dyn_resource("aws_s3_bucket", 1, block, std::sync::Arc::new(DynEchoArn))
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

    // Import by id produces an imported resource with the computed arn.
    let imported = svc
        .import_resource_state(Request::new(tfplugin6::import_resource_state::Request {
            type_name: "aws_s3_bucket".into(),
            id: "b2".into(),
            ..Default::default()
        }))
        .await
        .expect("import")
        .into_inner();
    assert!(
        imported.diagnostics.is_empty(),
        "{:?}",
        imported.diagnostics
    );
    assert_eq!(
        imported.imported_resources.len(),
        1,
        "one imported resource"
    );
    let state = imported.imported_resources[0]
        .state
        .as_ref()
        .expect("state");
    let value = terraform_codec::decode_msgpack(&state.msgpack, &cty).unwrap();
    let Value::Object(fields) = value else {
        panic!("imported state should be an object");
    };
    assert_eq!(fields["name"], Value::String("b2".into()), "imported by id");
    assert_eq!(fields["arn"], Value::String("arn:aws:s3:::b2".into()));

    // Upgrade from v0 state (old `bucket` field) migrates to the current schema.
    let old_state = br#"{"bucket":"b3"}"#;
    let upgraded = svc
        .upgrade_resource_state(Request::new(tfplugin6::upgrade_resource_state::Request {
            type_name: "aws_s3_bucket".into(),
            version: 0,
            raw_state: Some(tfplugin6::RawState {
                json: old_state.to_vec(),
                flatmap: std::collections::HashMap::new(),
            }),
        }))
        .await
        .expect("upgrade")
        .into_inner();
    assert!(
        upgraded.diagnostics.is_empty(),
        "{:?}",
        upgraded.diagnostics
    );
    let state = upgraded.upgraded_state.expect("upgraded state");
    let value = terraform_codec::decode_msgpack(&state.msgpack, &cty).unwrap();
    let Value::Object(fields) = value else {
        panic!("upgraded state should be an object");
    };
    assert_eq!(
        fields["name"],
        Value::String("b3".into()),
        "v0 `bucket` migrated to `name`"
    );
    assert_eq!(fields["arn"], Value::String("arn:aws:s3:::b3".into()));
}

/// A typed resource whose `create` fails with an attribute-pathed error and an
/// accompanying warning — proving CRUD handlers can do more than a flat error.
#[derive(Facet)]
#[facet(terraform::resource("failing"))]
#[allow(dead_code)]
struct Failing {
    name: String,
}

struct FailingResource;

#[async_trait]
impl Resource for FailingResource {
    type Model = Failing;

    async fn create(&self, _ctx: &mut Ctx, _planned: Failing) -> Result<Failing, ResourceError> {
        Err(ResourceError::new("create failed")
            .with_detail("the backend rejected it")
            .at(["name"])
            .with_warning(Diag::warning("deprecated", "`name` will be renamed")))
    }
}

#[tokio::test]
async fn crud_error_carries_attribute_path_and_warnings() {
    let svc = Provider::builder()
        .resource(FailingResource)
        .build()
        .map(ProviderService::new)
        .expect("provider builds");

    let ty = terraform_value::Type::Object(vec![terraform_value::ObjectAttr {
        name: "name".into(),
        ty: terraform_value::Type::String,
        optional: false,
    }]);
    let mut obj = std::collections::BTreeMap::new();
    obj.insert(
        "name".to_string(),
        terraform_value::Value::String("x".into()),
    );
    let planned = tfplugin6::DynamicValue {
        msgpack: terraform_codec::encode_msgpack(&terraform_value::Value::Object(obj), &ty)
            .unwrap(),
        json: vec![],
    };

    let apply = svc
        .apply_resource_change(Request::new(tfplugin6::apply_resource_change::Request {
            type_name: "failing".into(),
            prior_state: None,
            planned_state: Some(planned),
            ..Default::default()
        }))
        .await
        .expect("apply")
        .into_inner();

    // One error (pointed at `name`) plus one warning.
    assert_eq!(apply.diagnostics.len(), 2, "{:?}", apply.diagnostics);
    let error = &apply.diagnostics[0];
    assert_eq!(
        error.severity,
        tfplugin6::diagnostic::Severity::Error as i32
    );
    assert_eq!(error.summary, "create failed");
    let path = error
        .attribute
        .as_ref()
        .expect("error carries an attribute path");
    match path.steps[0].selector.as_ref().unwrap() {
        tfplugin6::attribute_path::step::Selector::AttributeName(n) => assert_eq!(n, "name"),
        other => panic!("expected an attribute-name step, got {other:?}"),
    }

    let warning = &apply.diagnostics[1];
    assert_eq!(
        warning.severity,
        tfplugin6::diagnostic::Severity::Warning as i32
    );
    assert_eq!(warning.summary, "deprecated");
}

/// A handler that panics in `create` — used to prove the runtime contains the
/// panic as a diagnostic rather than letting it abort the plugin process.
struct PanicOnCreate;

#[async_trait]
impl terraform_runtime::DynResource for PanicOnCreate {
    async fn create(
        &self,
        _planned: terraform_value::Value,
    ) -> Result<terraform_value::Value, terraform_runtime::Diagnostics> {
        panic!("boom in create");
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
    async fn import(
        &self,
        _id: String,
    ) -> Result<terraform_value::Value, terraform_runtime::Diagnostics> {
        Ok(terraform_value::Value::Null)
    }
    async fn upgrade(
        &self,
        _from_version: i64,
        prior: terraform_value::Value,
    ) -> Result<terraform_value::Value, terraform_runtime::Diagnostics> {
        Ok(prior)
    }
    async fn validate(&self, _config: terraform_value::Value) -> terraform_runtime::Diagnostics {
        Vec::new()
    }
}

#[tokio::test]
async fn handler_panic_becomes_diagnostic() {
    use terraform_ir::{AttributeSchema, Block};
    use terraform_value::{ObjectAttr, Type, Value};

    let block = Block {
        attributes: vec![AttributeSchema {
            required: true,
            ..AttributeSchema::new("name", Type::String)
        }],
        nested_blocks: vec![],
    };
    let provider = Provider::builder()
        .dyn_resource("boom", 1, block, std::sync::Arc::new(PanicOnCreate))
        .build()
        .expect("provider builds");
    let svc = ProviderService::new(provider);

    let cty = Type::Object(vec![ObjectAttr {
        name: "name".into(),
        ty: Type::String,
        optional: false,
    }]);
    let mut obj = std::collections::BTreeMap::new();
    obj.insert("name".to_string(), Value::String("b1".into()));
    let planned = tfplugin6::DynamicValue {
        msgpack: terraform_codec::encode_msgpack(&Value::Object(obj), &cty).unwrap(),
        json: vec![],
    };

    // Silence the default panic hook so the (expected) panic doesn't spam test
    // output; the runtime still catches it and reports a diagnostic.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let apply = svc
        .apply_resource_change(Request::new(tfplugin6::apply_resource_change::Request {
            type_name: "boom".into(),
            prior_state: None,
            planned_state: Some(planned),
            ..Default::default()
        }))
        .await
        .expect("apply call returns (not a process abort)")
        .into_inner();
    std::panic::set_hook(prev);

    assert_eq!(apply.diagnostics.len(), 1, "{:?}", apply.diagnostics);
    assert!(
        apply.diagnostics[0].summary.contains("panicked"),
        "summary was {:?}",
        apply.diagnostics[0].summary
    );
    assert!(
        apply.diagnostics[0].detail.contains("boom in create"),
        "detail was {:?}",
        apply.diagnostics[0].detail
    );
}

#[tokio::test]
async fn validate_resource_config_surfaces_diagnostics() {
    use terraform_ir::{AttributeSchema, Block};
    use terraform_value::{ObjectAttr, Type, Value};

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
        .dyn_resource("aws_s3_bucket", 1, block, std::sync::Arc::new(DynEchoArn))
        .build()
        .expect("provider builds");
    let svc = ProviderService::new(provider);

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
    let config = |name: &str| {
        let mut o = std::collections::BTreeMap::new();
        o.insert("name".to_string(), Value::String(name.into()));
        o.insert("arn".to_string(), Value::Null);
        tfplugin6::DynamicValue {
            msgpack: terraform_codec::encode_msgpack(&Value::Object(o), &cty).unwrap(),
            json: vec![],
        }
    };

    // A bad name yields one diagnostic, pointed at the `name` attribute.
    let bad = svc
        .validate_resource_config(Request::new(tfplugin6::validate_resource_config::Request {
            type_name: "aws_s3_bucket".into(),
            config: Some(config("bad")),
            ..Default::default()
        }))
        .await
        .expect("validate")
        .into_inner();
    assert_eq!(bad.diagnostics.len(), 1);
    assert_eq!(bad.diagnostics[0].summary, "invalid name");
    let path = bad.diagnostics[0]
        .attribute
        .as_ref()
        .expect("diagnostic carries an attribute path");
    match path.steps[0].selector.as_ref().unwrap() {
        tfplugin6::attribute_path::step::Selector::AttributeName(n) => assert_eq!(n, "name"),
        other => panic!("expected an attribute-name step, got {other:?}"),
    }

    // A good name validates clean.
    let ok = svc
        .validate_resource_config(Request::new(tfplugin6::validate_resource_config::Request {
            type_name: "aws_s3_bucket".into(),
            config: Some(config("ok")),
            ..Default::default()
        }))
        .await
        .expect("validate")
        .into_inner();
    assert!(ok.diagnostics.is_empty());
}

// --- Handler context: success-path warnings + private state ----------------

/// A model for the ctx probe.
#[derive(Facet)]
#[facet(terraform::resource("ctx_probe"))]
#[allow(dead_code)]
struct Probe {
    name: String,
}

/// Exercises the handler [`Ctx`]: `create` emits a success-path warning and
/// persists private state; `read` surfaces the incoming private state back
/// through the model so the test can observe it.
struct CtxProbeResource;

#[async_trait]
impl Resource for CtxProbeResource {
    type Model = Probe;

    async fn create(&self, ctx: &mut Ctx, planned: Probe) -> Result<Probe, ResourceError> {
        ctx.warn("deprecated", "`name` will be renamed soon");
        ctx.set_private(b"token-42".to_vec());
        Ok(planned)
    }

    async fn read(
        &self,
        ctx: &mut Ctx,
        mut current: Probe,
    ) -> Result<Option<Probe>, ResourceError> {
        // Echo the incoming private state into the model so the response reflects
        // what the handler observed.
        current.name = String::from_utf8_lossy(ctx.private()).into_owned();
        Ok(Some(current))
    }
}

fn probe_cty() -> terraform_value::Type {
    terraform_value::Type::Object(vec![terraform_value::ObjectAttr {
        name: "name".into(),
        ty: terraform_value::Type::String,
        optional: false,
    }])
}

fn probe_dv(name: &str) -> tfplugin6::DynamicValue {
    let mut obj = std::collections::BTreeMap::new();
    obj.insert(
        "name".to_string(),
        terraform_value::Value::String(name.into()),
    );
    tfplugin6::DynamicValue {
        msgpack: terraform_codec::encode_msgpack(
            &terraform_value::Value::Object(obj),
            &probe_cty(),
        )
        .unwrap(),
        json: vec![],
    }
}

#[tokio::test]
async fn create_success_carries_warning_and_persists_private_state() {
    let svc = Provider::builder()
        .resource(CtxProbeResource)
        .build()
        .map(ProviderService::new)
        .expect("provider builds");

    let apply = svc
        .apply_resource_change(Request::new(tfplugin6::apply_resource_change::Request {
            type_name: "ctx_probe".into(),
            prior_state: None,
            planned_state: Some(probe_dv("x")),
            ..Default::default()
        }))
        .await
        .expect("apply")
        .into_inner();

    // The successful create still surfaces the ctx warning...
    assert_eq!(apply.diagnostics.len(), 1, "{:?}", apply.diagnostics);
    assert_eq!(
        apply.diagnostics[0].severity,
        tfplugin6::diagnostic::Severity::Warning as i32
    );
    assert_eq!(apply.diagnostics[0].summary, "deprecated");
    // ...and persists the private state the handler set.
    assert_eq!(apply.private.as_slice(), b"token-42");
}

#[tokio::test]
async fn read_observes_incoming_private_state() {
    let svc = Provider::builder()
        .resource(CtxProbeResource)
        .build()
        .map(ProviderService::new)
        .expect("provider builds");

    let read = svc
        .read_resource(Request::new(tfplugin6::read_resource::Request {
            type_name: "ctx_probe".into(),
            current_state: Some(probe_dv("ignored")),
            private: b"seen-private".to_vec(),
            ..Default::default()
        }))
        .await
        .expect("read")
        .into_inner();

    // The handler read its private state and echoed it into `name`.
    let state = read.new_state.expect("new state");
    let value = terraform_codec::decode_msgpack(&state.msgpack, &probe_cty()).unwrap();
    let terraform_value::Value::Object(fields) = value else {
        panic!("state should be an object");
    };
    assert_eq!(
        fields["name"],
        terraform_value::Value::String("seen-private".into())
    );
    // The private state round-trips back out unchanged when the handler did not
    // replace it.
    assert_eq!(read.private.as_slice(), b"seen-private");
}

// --- Provider-defined functions --------------------------------------------

#[derive(Facet)]
#[allow(dead_code)]
struct AddArgs {
    a: i64,
    b: i64,
}

struct AddFn;

#[async_trait]
impl terraform_runtime::Function for AddFn {
    type Params = AddArgs;
    type Output = i64;

    async fn call(&self, p: AddArgs) -> Result<i64, terraform_runtime::FunctionError> {
        Ok(p.a + p.b)
    }
}

fn function_service() -> ProviderService {
    Provider::builder()
        .function("add", AddFn)
        .build()
        .map(ProviderService::new)
        .expect("provider builds")
}

#[tokio::test]
async fn get_functions_lists_the_signature() {
    let resp = function_service()
        .get_functions(Request::new(tfplugin6::get_functions::Request {}))
        .await
        .expect("GetFunctions")
        .into_inner();
    let add = resp.functions.get("add").expect("add function present");
    assert_eq!(add.parameters.len(), 2);
    assert!(add.r#return.is_some(), "return type is published");
}

#[tokio::test]
async fn call_function_decodes_args_and_encodes_result() {
    let num_ty = terraform_value::Type::Number;
    let arg = |n: i64| tfplugin6::DynamicValue {
        msgpack: terraform_codec::encode_msgpack(&terraform_value::Value::from(n), &num_ty)
            .unwrap(),
        json: vec![],
    };
    let resp = function_service()
        .call_function(Request::new(tfplugin6::call_function::Request {
            name: "add".into(),
            arguments: vec![arg(2), arg(40)],
        }))
        .await
        .expect("CallFunction")
        .into_inner();

    assert!(resp.error.is_none(), "{:?}", resp.error);
    let result = resp.result.expect("result present");
    let value = terraform_codec::decode_msgpack(&result.msgpack, &num_ty).unwrap();
    assert_eq!(value, terraform_value::Value::from(42_i64));
}

#[tokio::test]
async fn call_unknown_function_returns_a_function_error() {
    let resp = function_service()
        .call_function(Request::new(tfplugin6::call_function::Request {
            name: "nope".into(),
            arguments: vec![],
        }))
        .await
        .expect("CallFunction")
        .into_inner();
    assert!(resp.result.is_none());
    assert!(resp.error.is_some(), "an unknown function is an error");
}

/// A variadic function with a `String` leading parameter and `i64` trailing
/// arguments — proving the const and variadic args can be different types.
#[derive(Facet)]
#[allow(dead_code)]
struct LabelArgs {
    label: String,
}

struct LabelFn;

#[async_trait]
impl terraform_runtime::VariadicFunction for LabelFn {
    type Params = LabelArgs;
    type VarArg = i64;
    type Output = String;

    async fn call(
        &self,
        p: LabelArgs,
        nums: Vec<i64>,
    ) -> Result<String, terraform_runtime::FunctionError> {
        Ok(format!("{}: {:?}", p.label, nums))
    }
}

#[tokio::test]
async fn variadic_function_splits_heterogeneous_leading_and_trailing_args() {
    let svc = Provider::builder()
        .function_variadic("label", LabelFn)
        .build()
        .map(ProviderService::new)
        .expect("provider builds");

    let str_ty = terraform_value::Type::String;
    let num_ty = terraform_value::Type::Number;
    let str_arg = |s: &str| tfplugin6::DynamicValue {
        msgpack: terraform_codec::encode_msgpack(
            &terraform_value::Value::String(s.into()),
            &str_ty,
        )
        .unwrap(),
        json: vec![],
    };
    let num_arg = |n: i64| tfplugin6::DynamicValue {
        msgpack: terraform_codec::encode_msgpack(&terraform_value::Value::from(n), &num_ty)
            .unwrap(),
        json: vec![],
    };
    let call = |args: Vec<tfplugin6::DynamicValue>| {
        svc.call_function(Request::new(tfplugin6::call_function::Request {
            name: "label".into(),
            arguments: args,
        }))
    };
    let decode = |resp: tfplugin6::call_function::Response| {
        terraform_codec::decode_msgpack(&resp.result.expect("result").msgpack, &str_ty).unwrap()
    };

    // Leading "xs" (String) + three trailing i64s collected into the Vec.
    let resp = call(vec![str_arg("xs"), num_arg(1), num_arg(2), num_arg(3)])
        .await
        .expect("CallFunction")
        .into_inner();
    assert!(resp.error.is_none(), "{:?}", resp.error);
    assert_eq!(
        decode(resp),
        terraform_value::Value::String("xs: [1, 2, 3]".into())
    );

    // Zero trailing arguments is valid (variadic = *zero* or more).
    let resp = call(vec![str_arg("ys")])
        .await
        .expect("CallFunction")
        .into_inner();
    assert!(resp.error.is_none(), "{:?}", resp.error);
    assert_eq!(
        decode(resp),
        terraform_value::Value::String("ys: []".into())
    );
}

// --- Ephemeral resources ---------------------------------------------------

/// An ephemeral auth token: `open` mints one bound to `role`, stashes the role in
/// private state (the only thing `renew`/`close` receive), and asks for renewal.
#[derive(Facet)]
#[facet(terraform::ephemeral("auth_token"))]
#[allow(dead_code)]
struct Token {
    role: String,
    #[facet(terraform::computed)]
    token: String,
}

struct TokenEphemeral;

#[async_trait]
impl Ephemeral for TokenEphemeral {
    type Model = Token;

    async fn open(&self, ctx: &mut Ctx, mut config: Token) -> Result<Token, EphemeralError> {
        config.token = format!("tok-{}", config.role);
        ctx.set_private(config.role.clone().into_bytes());
        ctx.renew_after(std::time::Duration::from_secs(300));
        Ok(config)
    }

    async fn renew(&self, ctx: &mut Ctx) -> Result<(), EphemeralError> {
        // The handle is the role stashed on open; re-arm the renewal window.
        if ctx.private().is_empty() {
            return Err(EphemeralError::new("renew got no handle"));
        }
        ctx.renew_after(std::time::Duration::from_secs(300));
        Ok(())
    }

    async fn close(&self, ctx: &mut Ctx) -> Result<(), EphemeralError> {
        if ctx.private().is_empty() {
            return Err(EphemeralError::new("close got no handle"));
        }
        Ok(())
    }
}

/// The `cty` object type for the `Token` ephemeral resource's config/result.
fn token_cty() -> terraform_value::Type {
    terraform_value::Type::Object(vec![
        terraform_value::ObjectAttr {
            name: "role".into(),
            ty: terraform_value::Type::String,
            optional: false,
        },
        terraform_value::ObjectAttr {
            name: "token".into(),
            ty: terraform_value::Type::String,
            optional: true,
        },
    ])
}

fn token_config(role: &str) -> tfplugin6::DynamicValue {
    let mut obj = std::collections::BTreeMap::new();
    obj.insert(
        "role".to_string(),
        terraform_value::Value::String(role.into()),
    );
    obj.insert("token".to_string(), terraform_value::Value::Null);
    tfplugin6::DynamicValue {
        msgpack: terraform_codec::encode_msgpack(
            &terraform_value::Value::Object(obj),
            &token_cty(),
        )
        .unwrap(),
        json: vec![],
    }
}

fn ephemeral_service() -> ProviderService {
    Provider::builder()
        .ephemeral(TokenEphemeral)
        .build()
        .map(ProviderService::new)
        .expect("provider builds")
}

#[tokio::test]
async fn open_ephemeral_fills_result_sets_private_and_renew_at() {
    let svc = ephemeral_service();

    let resp = svc
        .open_ephemeral_resource(Request::new(tfplugin6::open_ephemeral_resource::Request {
            type_name: "auth_token".into(),
            config: Some(token_config("admin")),
            ..Default::default()
        }))
        .await
        .expect("open")
        .into_inner();

    assert!(resp.diagnostics.is_empty(), "{:?}", resp.diagnostics);

    // The computed result is filled and encoded back.
    let result = resp.result.expect("result");
    let value = terraform_codec::decode_msgpack(&result.msgpack, &token_cty()).unwrap();
    let terraform_value::Value::Object(fields) = value else {
        panic!("result should be an object");
    };
    assert_eq!(
        fields["token"],
        terraform_value::Value::String("tok-admin".into())
    );

    // The handle was stashed and a renewal deadline was requested.
    assert_eq!(resp.private.as_deref(), Some(b"admin".as_slice()));
    assert!(resp.renew_at.is_some(), "open requested a renewal time");
}

#[tokio::test]
async fn renew_ephemeral_echoes_handle_and_refreshes_deadline() {
    let svc = ephemeral_service();

    let resp = svc
        .renew_ephemeral_resource(Request::new(tfplugin6::renew_ephemeral_resource::Request {
            type_name: "auth_token".into(),
            private: Some(b"admin".to_vec()),
        }))
        .await
        .expect("renew")
        .into_inner();

    assert!(resp.diagnostics.is_empty(), "{:?}", resp.diagnostics);
    // A handler that didn't rewrite private state keeps the incoming handle.
    assert_eq!(resp.private.as_deref(), Some(b"admin".as_slice()));
    assert!(resp.renew_at.is_some(), "renew pushed the deadline forward");
}

#[tokio::test]
async fn close_ephemeral_reads_handle_and_surfaces_errors() {
    let svc = ephemeral_service();

    // With a handle: clean close.
    let ok = svc
        .close_ephemeral_resource(Request::new(tfplugin6::close_ephemeral_resource::Request {
            type_name: "auth_token".into(),
            private: Some(b"admin".to_vec()),
        }))
        .await
        .expect("close")
        .into_inner();
    assert!(ok.diagnostics.is_empty(), "{:?}", ok.diagnostics);

    // Without a handle: the handler errors, surfaced as a diagnostic.
    let missing = svc
        .close_ephemeral_resource(Request::new(tfplugin6::close_ephemeral_resource::Request {
            type_name: "auth_token".into(),
            private: None,
        }))
        .await
        .expect("close")
        .into_inner();
    assert_eq!(missing.diagnostics.len(), 1, "missing handle is an error");

    // An unknown type name is rejected.
    let unknown = svc
        .open_ephemeral_resource(Request::new(tfplugin6::open_ephemeral_resource::Request {
            type_name: "nope".into(),
            config: Some(token_config("x")),
            ..Default::default()
        }))
        .await
        .expect("open")
        .into_inner();
    assert_eq!(
        unknown.diagnostics.len(),
        1,
        "unknown ephemeral is an error"
    );
}

/// A managed resource with observable create/delete, exposed as an ephemeral
/// resource via [`EphemeralFromResource`] — proving Open→create, Close→delete,
/// and the private-state round-trip the wrapper uses to hand the model to delete.
#[derive(Facet)]
#[facet(terraform::resource("temp_rule"))]
#[allow(dead_code)]
struct TempRule {
    cidr: String,
    #[facet(terraform::computed)]
    id: String,
}

struct TempRuleResource {
    log: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait]
impl Resource for TempRuleResource {
    type Model = TempRule;

    async fn create(
        &self,
        _ctx: &mut Ctx,
        mut planned: TempRule,
    ) -> Result<TempRule, ResourceError> {
        self.log
            .lock()
            .unwrap()
            .push(format!("create {}", planned.cidr));
        planned.id = format!("rule-{}", planned.cidr);
        Ok(planned)
    }

    async fn delete(&self, _ctx: &mut Ctx, prior: TempRule) -> Result<(), ResourceError> {
        self.log
            .lock()
            .unwrap()
            .push(format!("delete {}", prior.id));
        Ok(())
    }
}

fn temp_rule_cty() -> terraform_value::Type {
    terraform_value::Type::Object(vec![
        terraform_value::ObjectAttr {
            name: "cidr".into(),
            ty: terraform_value::Type::String,
            optional: false,
        },
        terraform_value::ObjectAttr {
            name: "id".into(),
            ty: terraform_value::Type::String,
            optional: true,
        },
    ])
}

#[tokio::test]
async fn ephemeral_from_resource_runs_create_on_open_and_delete_on_close() {
    let log = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    // No ephemeral marker on `TempRule`, so it registers under snake_case of the
    // identifier: `temp_rule`.
    let svc = Provider::builder()
        .ephemeral(EphemeralFromResource(TempRuleResource { log: log.clone() }))
        .build()
        .map(ProviderService::new)
        .expect("provider builds");

    let mut cfg = std::collections::BTreeMap::new();
    cfg.insert(
        "cidr".to_string(),
        terraform_value::Value::String("10.0.0.0/8".into()),
    );
    cfg.insert("id".to_string(), terraform_value::Value::Null);
    let config = tfplugin6::DynamicValue {
        msgpack: terraform_codec::encode_msgpack(
            &terraform_value::Value::Object(cfg),
            &temp_rule_cty(),
        )
        .unwrap(),
        json: vec![],
    };

    let opened = svc
        .open_ephemeral_resource(Request::new(tfplugin6::open_ephemeral_resource::Request {
            type_name: "temp_rule".into(),
            config: Some(config),
            ..Default::default()
        }))
        .await
        .expect("open")
        .into_inner();
    assert!(opened.diagnostics.is_empty(), "{:?}", opened.diagnostics);

    // create() filled the computed id.
    let value =
        terraform_codec::decode_msgpack(&opened.result.expect("result").msgpack, &temp_rule_cty())
            .unwrap();
    let terraform_value::Value::Object(fields) = value else {
        panic!("result should be an object");
    };
    assert_eq!(
        fields["id"],
        terraform_value::Value::String("rule-10.0.0.0/8".into())
    );

    // The wrapper stashed the created model as JSON so close can reconstruct it.
    let private = opened.private.expect("wrapper recorded the handle");
    let closed = svc
        .close_ephemeral_resource(Request::new(tfplugin6::close_ephemeral_resource::Request {
            type_name: "temp_rule".into(),
            private: Some(private),
        }))
        .await
        .expect("close")
        .into_inner();
    assert!(closed.diagnostics.is_empty(), "{:?}", closed.diagnostics);

    // Open ran create; Close ran delete with the reconstructed model.
    let log = log.lock().unwrap();
    assert_eq!(
        *log,
        vec![
            "create 10.0.0.0/8".to_string(),
            "delete rule-10.0.0.0/8".to_string()
        ]
    );
}

/// A resource that accepts cross-type state moves from `legacy_widget`, mapping
/// the source's untyped `label` onto its own `name`. Proves the
/// `MoveResourceState` RPC decodes foreign source state and runs `move_state`.
#[derive(Facet)]
#[facet(terraform::resource("widget"))]
#[allow(dead_code)]
struct Widget {
    name: String,
}

struct WidgetResource;

#[async_trait]
impl Resource for WidgetResource {
    type Model = Widget;

    async fn create(&self, _ctx: &mut Ctx, planned: Widget) -> Result<Widget, ResourceError> {
        Ok(planned)
    }

    async fn move_state(
        &self,
        _ctx: &mut Ctx,
        source_type_name: String,
        source_state: terraform_value::Value,
    ) -> Result<Widget, ResourceError> {
        if source_type_name != "legacy_widget" {
            return Err(ResourceError::new(format!(
                "cannot move from `{source_type_name}`"
            )));
        }
        // The source state is untyped (its schema is foreign): pull `label` out.
        let terraform_value::Value::Object(fields) = &source_state else {
            return Err(ResourceError::new("source state is not an object"));
        };
        let Some(terraform_value::Value::String(label)) = fields.get("label") else {
            return Err(ResourceError::new("source state is missing `label`"));
        };
        Ok(Widget {
            name: label.clone(),
        })
    }
}

#[tokio::test]
async fn move_resource_state_migrates_across_types() {
    let svc = Provider::builder()
        .resource(WidgetResource)
        .build()
        .map(ProviderService::new)
        .expect("provider builds");

    let ty = terraform_value::Type::Object(vec![terraform_value::ObjectAttr {
        name: "name".into(),
        ty: terraform_value::Type::String,
        optional: false,
    }]);

    let resp = svc
        .move_resource_state(Request::new(tfplugin6::move_resource_state::Request {
            source_type_name: "legacy_widget".into(),
            target_type_name: "widget".into(),
            source_state: Some(tfplugin6::RawState {
                json: br#"{"label":"hello"}"#.to_vec(),
                ..Default::default()
            }),
            ..Default::default()
        }))
        .await
        .expect("move")
        .into_inner();

    assert!(resp.diagnostics.is_empty(), "{:?}", resp.diagnostics);
    let target = resp.target_state.expect("target state");
    let target = terraform_codec::decode_msgpack(&target.msgpack, &ty).expect("decode target");
    let terraform_value::Value::Object(fields) = target else {
        panic!("target should be an object");
    };
    assert_eq!(
        fields["name"],
        terraform_value::Value::String("hello".into()),
        "the source `label` should migrate into the target `name`"
    );
}

#[tokio::test]
async fn move_resource_state_unsupported_yields_diagnostic() {
    // `WoResource` does not implement `move_state`, so the defaulted hook errors.
    let svc = Provider::builder()
        .resource(WoResource {
            seen: Arc::new(std::sync::Mutex::new(None)),
        })
        .build()
        .map(ProviderService::new)
        .expect("provider builds");

    let resp = svc
        .move_resource_state(Request::new(tfplugin6::move_resource_state::Request {
            source_type_name: "legacy".into(),
            target_type_name: "wo_secret".into(),
            source_state: Some(tfplugin6::RawState {
                json: br#"{"name":"a"}"#.to_vec(),
                ..Default::default()
            }),
            ..Default::default()
        }))
        .await
        .expect("move")
        .into_inner();

    assert!(
        !resp.diagnostics.is_empty(),
        "an unsupported move must surface a diagnostic"
    );
    assert!(resp.target_state.is_none());
}

/// A resource that declares a resource **identity** (the computed `id`). Proves
/// `GetResourceIdentitySchemas` reports the reflected identity schema.
#[derive(Facet)]
#[facet(terraform::resource("ident_widget"))]
#[allow(dead_code)]
struct IdentWidget {
    name: String,
    #[facet(terraform::computed)]
    #[facet(terraform::identity)]
    id: String,
}

struct IdentWidgetResource;

#[async_trait]
impl Resource for IdentWidgetResource {
    type Model = IdentWidget;

    async fn create(
        &self,
        _ctx: &mut Ctx,
        planned: IdentWidget,
    ) -> Result<IdentWidget, ResourceError> {
        Ok(planned)
    }
}

#[tokio::test]
async fn get_resource_identity_schemas_reports_declared_identity() {
    let svc = Provider::builder()
        .resource(IdentWidgetResource)
        .build()
        .map(ProviderService::new)
        .expect("provider builds");

    let resp = svc
        .get_resource_identity_schemas(Request::new(
            tfplugin6::get_resource_identity_schemas::Request::default(),
        ))
        .await
        .expect("identity schemas")
        .into_inner();

    assert!(resp.diagnostics.is_empty(), "{:?}", resp.diagnostics);
    let identity = resp
        .identity_schemas
        .get("ident_widget")
        .expect("ident_widget has an identity schema");
    assert_eq!(identity.identity_attributes.len(), 1);
    assert_eq!(identity.identity_attributes[0].name, "id");
    assert!(
        identity.identity_attributes[0].required_for_import,
        "identity attribute is required for import"
    );
}
