//! Node native addon: a thin napi-rs bridge over the dynamic provider seam in
//! `terraform-runtime`.
//!
//! JavaScript supplies, per resource/data source, a schema description (cty-typed
//! attributes as JSON) and async lifecycle handlers. This crate:
//!
//! - parses the schema JSON into a `terraform-ir` [`Block`];
//! - implements the erased [`DynResource`]/[`DynDataSource`] traits by calling
//!   the JS handlers (marshalling the dynamic `Value` to/from JSON across the
//!   boundary, typed by the schema); and
//! - runs the real tfplugin6 server in-process via `terraform_runtime::serve`.
//!
//! All Terraform/protocol concerns stay in Rust; JS only sees decoded values.

#![allow(unsafe_code)]

use std::sync::Arc;

use napi::bindgen_prelude::*;
use napi::threadsafe_function::ThreadsafeFunction;
use napi_derive::napi;

use terraform_codec::{decode_json, encode_json};
use terraform_ir::{AttributeSchema, Block};
use terraform_runtime::{
    async_trait, serve as serve_provider, Diag, Diagnostics, DynConfigure, DynDataSource,
    DynResource, Provider as RtProvider,
};
use terraform_value::{Type, Value};

/// An async JS handler: takes a JSON string, returns a `Promise<string>`.
///
/// Note: napi's type-def generator can't see through this alias, so the
/// `#[napi]` method signatures below spell the type out in full (which makes the
/// generated `index.d.ts` correct); this alias is only for the Rust internals.
type Handler = ThreadsafeFunction<String, Promise<String>>;

// --- marshalling ------------------------------------------------------------
//
// Both directions go through facet (no hand-rolled JSON): `encode_json` lowers a
// `Value` to a dynamic `facet_value::Value` which `facet_json` serializes, and
// the reverse parses with `facet_json` then types it with `decode_json` under
// the attribute's cty `Type`.

/// Serialize a dynamic [`Value`] to a JSON string for the JS boundary.
fn value_to_json(value: &Value) -> std::result::Result<String, String> {
    facet_json::to_string(&encode_json(value)).map_err(|e| e.to_string())
}

/// Parse a handler's JSON result back into a typed [`Value`] under `ty`.
fn json_to_value(json: &str, ty: &Type) -> std::result::Result<Value, String> {
    let parsed: facet_value::Value =
        facet_json::from_slice(json.as_bytes()).map_err(|e| e.to_string())?;
    decode_json(&parsed, ty).map_err(|e| e.to_string())
}

/// Build a [`Block`] from the JS schema description:
/// `{ "attributes": [ { "name", "type": <cty-json>, "required"?, "optional"?,
/// "computed"?, "forceNew"?, "sensitive"?, "description"? }, ... ] }`.
fn block_from_schema_json(json: &str) -> std::result::Result<Block, String> {
    let parsed: facet_value::Value =
        facet_json::from_slice(json.as_bytes()).map_err(|e| e.to_string())?;
    let obj = parsed.as_object().ok_or("schema must be a JSON object")?;
    let attrs = obj
        .get("attributes")
        .and_then(|v| v.as_array())
        .ok_or("schema must have an `attributes` array")?;

    let mut attributes = Vec::with_capacity(attrs.len());
    for attr in attrs.iter() {
        let ao = attr.as_object().ok_or("each attribute must be an object")?;
        let name = ao
            .get("name")
            .and_then(|v| v.as_string())
            .ok_or("attribute is missing a string `name`")?
            .as_str()
            .to_string();
        let type_val = ao
            .get("type")
            .ok_or_else(|| format!("attribute `{name}` is missing a `type`"))?;
        // The `type` field is a cty type constraint (itself JSON); re-serialize
        // it and reuse the canonical cty decoder.
        let type_bytes = facet_json::to_string(type_val).map_err(|e| e.to_string())?;
        let ty = Type::from_cty_json_bytes(type_bytes.as_bytes())?;
        let flag = |key: &str| ao.get(key).and_then(|v| v.as_bool()).unwrap_or(false);
        attributes.push(AttributeSchema {
            name,
            ty,
            description: ao
                .get("description")
                .and_then(|v| v.as_string())
                .map(|s| s.as_str().to_string()),
            required: flag("required"),
            optional: flag("optional"),
            computed: flag("computed"),
            sensitive: flag("sensitive"),
            force_new: flag("forceNew"),
            // The TS layer applies its own defaults; the Rust seam takes none.
            default: None,
        });
    }
    Ok(Block {
        attributes,
        nested_blocks: Vec::new(),
    })
}

fn handler_err(ctx: &str, e: impl std::fmt::Display) -> Diagnostics {
    vec![Diag::error(format!("{ctx} handler failed"), e.to_string())]
}

/// Parse a validate handler's JSON result — an array of
/// `{ severity, summary, detail?, attribute? }` — into [`Diagnostics`].
fn parse_diagnostics(json: &str) -> Diagnostics {
    let parsed: facet_value::Value = match facet_json::from_slice(json.as_bytes()) {
        Ok(v) => v,
        Err(e) => return handler_err("validate", e),
    };
    let Some(items) = parsed.as_array() else {
        return Vec::new();
    };
    let text = |obj: &facet_value::Value, key: &str| {
        obj.as_object()
            .and_then(|o| o.get(key))
            .and_then(|v| v.as_string())
            .map(|s| s.as_str().to_string())
            .unwrap_or_default()
    };
    items
        .iter()
        .map(|item| {
            let attribute = item
                .as_object()
                .and_then(|o| o.get("attribute"))
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_string().map(|s| s.as_str().to_string()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let (summary, detail) = (text(item, "summary"), text(item, "detail"));
            let diag = if text(item, "severity") == "warning" {
                Diag::warning(summary, detail)
            } else {
                Diag::error(summary, detail)
            };
            diag.at(attribute)
        })
        .collect()
}

/// Invoke an async JS handler with `input` and await its JSON result.
async fn call_handler(
    handler: &Handler,
    input: &Value,
    ctx: &str,
) -> std::result::Result<String, Diagnostics> {
    let json = value_to_json(input).map_err(|e| handler_err(ctx, e))?;
    let promise = handler
        .call_async(Ok(json))
        .await
        .map_err(|e| handler_err(ctx, e))?;
    promise.await.map_err(|e| handler_err(ctx, e))
}

// --- erased handlers backed by JS ------------------------------------------

/// A resource whose lifecycle is a set of async JS callbacks.
struct JsResource {
    /// The resource's cty object type, used to type handler results.
    ty: Type,
    create: Handler,
    read: Handler,
    update: Handler,
    delete: Handler,
    import: Handler,
    upgrade: Handler,
    validate: Handler,
}

#[async_trait]
impl DynResource for JsResource {
    async fn create(&self, planned: Value) -> std::result::Result<Value, Diagnostics> {
        let out = call_handler(&self.create, &planned, "create").await?;
        json_to_value(&out, &self.ty).map_err(|e| handler_err("create", e))
    }

    async fn read(&self, current: Value) -> std::result::Result<Option<Value>, Diagnostics> {
        let out = call_handler(&self.read, &current, "read").await?;
        // A JSON `null` result means the resource no longer exists.
        if out.trim() == "null" {
            return Ok(None);
        }
        json_to_value(&out, &self.ty)
            .map(Some)
            .map_err(|e| handler_err("read", e))
    }

    async fn update(
        &self,
        planned: Value,
        prior: Value,
    ) -> std::result::Result<Value, Diagnostics> {
        // The update handler sees both states as `{ planned, prior }`.
        let input = Value::Object(
            [
                ("planned".to_string(), planned),
                ("prior".to_string(), prior),
            ]
            .into_iter()
            .collect(),
        );
        let out = call_handler(&self.update, &input, "update").await?;
        json_to_value(&out, &self.ty).map_err(|e| handler_err("update", e))
    }

    async fn delete(&self, prior: Value) -> std::result::Result<(), Diagnostics> {
        call_handler(&self.delete, &prior, "delete").await?;
        Ok(())
    }

    async fn import(&self, id: String) -> std::result::Result<Value, Diagnostics> {
        // The import handler's input is the raw ID string (not a marshalled
        // Value), so it is passed through directly.
        let promise = self
            .import
            .call_async(Ok(id))
            .await
            .map_err(|e| handler_err("import", e))?;
        let out = promise.await.map_err(|e| handler_err("import", e))?;
        json_to_value(&out, &self.ty).map_err(|e| handler_err("import", e))
    }

    async fn upgrade(
        &self,
        from_version: i64,
        prior: Value,
    ) -> std::result::Result<Value, Diagnostics> {
        // The upgrade handler sees `{ fromVersion, priorState }`; the prior state
        // is the untyped stored state.
        let input = Value::Object(
            [
                ("fromVersion".to_string(), Value::from(from_version)),
                ("priorState".to_string(), prior),
            ]
            .into_iter()
            .collect(),
        );
        let out = call_handler(&self.upgrade, &input, "upgrade").await?;
        json_to_value(&out, &self.ty).map_err(|e| handler_err("upgrade", e))
    }

    async fn validate(&self, config: Value) -> Diagnostics {
        match call_handler(&self.validate, &config, "validate").await {
            Ok(out) => parse_diagnostics(&out),
            Err(diags) => diags,
        }
    }
}

/// A data source whose read is one async JS callback. Singular vs plural is
/// decided entirely by the JS-supplied schema and what the handler returns; the
/// Rust side treats both uniformly.
struct JsDataSource {
    ty: Type,
    read: Handler,
    validate: Handler,
}

#[async_trait]
impl DynDataSource for JsDataSource {
    async fn read(&self, config: Value) -> std::result::Result<Value, Diagnostics> {
        let out = call_handler(&self.read, &config, "data source read").await?;
        json_to_value(&out, &self.ty).map_err(|e| handler_err("data source read", e))
    }

    async fn validate(&self, config: Value) -> Diagnostics {
        match call_handler(&self.validate, &config, "data source validate").await {
            Ok(out) => parse_diagnostics(&out),
            Err(diags) => diags,
        }
    }
}

/// The provider-configure callback: a single async JS handler that receives the
/// decoded provider config (and sets up JS-side state the handlers read).
struct JsConfigure {
    configure: Handler,
}

#[async_trait]
impl DynConfigure for JsConfigure {
    async fn configure(&self, config: Value) -> std::result::Result<(), Diagnostics> {
        call_handler(&self.configure, &config, "configure").await?;
        Ok(())
    }
}

// --- the JS-facing provider -------------------------------------------------

/// A registered resource, ready to hand to the builder at serve time.
struct ResourceReg {
    name: String,
    version: i64,
    block: Block,
    handler: Arc<dyn DynResource>,
}

/// A registered data source.
struct DataSourceReg {
    name: String,
    block: Block,
    handler: Arc<dyn DynDataSource>,
}

/// The provider-level config block and its configure handler.
struct ProviderConfigReg {
    block: Block,
    handler: Arc<dyn DynConfigure>,
}

/// The provider definition assembled from JS, then served over tfplugin6.
#[napi]
pub struct Provider {
    resources: Vec<ResourceReg>,
    data_sources: Vec<DataSourceReg>,
    config: Option<ProviderConfigReg>,
}

fn napi_err(e: impl std::fmt::Display) -> Error {
    Error::from_reason(e.to_string())
}

#[napi]
impl Provider {
    /// Create an empty provider. (Constructed from JS as `new Provider()`; a
    /// Rust `Default` impl would be meaningless here.)
    #[napi(constructor)]
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Provider {
            resources: Vec::new(),
            data_sources: Vec::new(),
            config: None,
        }
    }

    /// Declare the provider's configuration block and the async `configure`
    /// handler that receives it on `ConfigureProvider`. The handler typically
    /// stores state the resource/data-source handlers close over.
    #[napi]
    pub fn config(
        &mut self,
        schema_json: String,
        configure: ThreadsafeFunction<String, Promise<String>>,
    ) -> Result<()> {
        let block = block_from_schema_json(&schema_json).map_err(napi_err)?;
        self.config = Some(ProviderConfigReg {
            block,
            handler: Arc::new(JsConfigure { configure }),
        });
        Ok(())
    }

    /// Register a managed resource: its `type_name`, a schema description
    /// (`schema_json`), and the four async lifecycle handlers.
    #[napi]
    #[allow(clippy::too_many_arguments)]
    pub fn resource(
        &mut self,
        type_name: String,
        version: i32,
        schema_json: String,
        create: ThreadsafeFunction<String, Promise<String>>,
        read: ThreadsafeFunction<String, Promise<String>>,
        update: ThreadsafeFunction<String, Promise<String>>,
        delete: ThreadsafeFunction<String, Promise<String>>,
        import: ThreadsafeFunction<String, Promise<String>>,
        upgrade: ThreadsafeFunction<String, Promise<String>>,
        validate: ThreadsafeFunction<String, Promise<String>>,
    ) -> Result<()> {
        let block = block_from_schema_json(&schema_json).map_err(napi_err)?;
        let ty = block.cty_type();
        self.resources.push(ResourceReg {
            name: type_name,
            version: version as i64,
            block,
            handler: Arc::new(JsResource {
                ty,
                create,
                read,
                update,
                delete,
                import,
                upgrade,
                validate,
            }),
        });
        Ok(())
    }

    /// Register a data source: its `type_name`, a schema description, and a
    /// single async `read` handler. The schema and handler together decide the
    /// shape (a singular object, or a plural `results` list).
    #[napi]
    pub fn data_source(
        &mut self,
        type_name: String,
        schema_json: String,
        read: ThreadsafeFunction<String, Promise<String>>,
        validate: ThreadsafeFunction<String, Promise<String>>,
    ) -> Result<()> {
        let block = block_from_schema_json(&schema_json).map_err(napi_err)?;
        let ty = block.cty_type();
        self.data_sources.push(DataSourceReg {
            name: type_name,
            block,
            handler: Arc::new(JsDataSource { ty, read, validate }),
        });
        Ok(())
    }

    /// Serve the provider over the Terraform plugin protocol. Performs the
    /// go-plugin handshake on stdout and runs until SIGTERM, so the returned
    /// promise stays pending for the provider's lifetime.
    #[napi]
    pub async fn serve(&self) -> Result<()> {
        let mut builder = RtProvider::builder();
        if let Some(cfg) = &self.config {
            builder = builder
                .dyn_provider_config(cfg.block.clone())
                .dyn_configure(Arc::clone(&cfg.handler));
        }
        for r in &self.resources {
            builder = builder.dyn_resource(
                r.name.clone(),
                r.version,
                r.block.clone(),
                Arc::clone(&r.handler),
            );
        }
        for d in &self.data_sources {
            builder =
                builder.dyn_data_source(d.name.clone(), d.block.clone(), Arc::clone(&d.handler));
        }
        let provider = builder.build().map_err(napi_err)?;
        serve_provider(provider).await.map_err(napi_err)
    }
}
