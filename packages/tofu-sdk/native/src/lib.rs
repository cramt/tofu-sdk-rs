//! Node native addon: a thin napi-rs bridge over the dynamic provider seam in
//! `terraform-runtime`.
//!
//! JavaScript supplies, per resource/data source, a schema description (cty-typed
//! attributes as JSON) and async lifecycle handlers. This crate:
//!
//! - parses the schema JSON into a `terraform-ir` [`Block`];
//! - implements the erased [`DynResource`]/[`DynDataSource`]/[`DynEphemeral`]
//!   traits by calling the JS handlers (marshalling the dynamic `Value` to/from
//!   JSON across the boundary, typed by the schema); and
//! - runs the real tfplugin6 server in-process via `terraform_runtime::serve`.
//!
//! All Terraform/protocol concerns stay in Rust; JS only sees decoded values.

#![allow(unsafe_code)]

use std::sync::Arc;

use napi::bindgen_prelude::*;
use napi::threadsafe_function::ThreadsafeFunction;
use napi_derive::napi;

use std::time::{Duration, SystemTime};

use terraform_codec::{decode_json, encode_json};
use terraform_ir::{AttributeSchema, Block, FunctionSignature, NestedBlock, NestingMode, Parameter};
use terraform_runtime::{
    async_trait, current_ctx, serve as serve_provider, Diag, Diagnostics, DynConfigure,
    DynDataSource, DynEphemeral, DynFunction, DynResource, DynValidateConfig, FunctionError,
    Provider as RtProvider,
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
/// "computed"?, "forceNew"?, "sensitive"?, "writeOnly"?, "deprecated"?,
/// "description"? }, ... ],
///   "blocks"?: [ { "name", "nesting"?, "minItems"?, "maxItems"?, "block": <schema> }, ... ] }`.
fn block_from_schema_json(json: &str) -> std::result::Result<Block, String> {
    let parsed: facet_value::Value =
        facet_json::from_slice(json.as_bytes()).map_err(|e| e.to_string())?;
    block_from_schema_value(&parsed)
}

/// Build a [`Block`] from an already-parsed schema object. Factored out of
/// [`block_from_schema_json`] so nested blocks (whose `block` field is itself a
/// schema object) can recurse.
fn block_from_schema_value(parsed: &facet_value::Value) -> std::result::Result<Block, String> {
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
            write_only: flag("writeOnly"),
            force_new: flag("forceNew"),
            deprecated: flag("deprecated").then(String::new),
            // The TS layer applies its own defaults; the Rust seam takes none.
            default: None,
        });
    }

    // Nested blocks are optional; absent `blocks` means a leaf block.
    let mut nested_blocks = Vec::new();
    if let Some(blocks) = obj.get("blocks").and_then(|v| v.as_array()) {
        for b in blocks.iter() {
            nested_blocks.push(nested_block_from_value(b)?);
        }
    }

    Ok(Block {
        attributes,
        nested_blocks,
    })
}

/// Parse one nested-block descriptor:
/// `{ "name", "nesting"?, "minItems"?, "maxItems"?, "block": <schema> }`.
fn nested_block_from_value(value: &facet_value::Value) -> std::result::Result<NestedBlock, String> {
    let bo = value.as_object().ok_or("each block must be an object")?;
    let name = bo
        .get("name")
        .and_then(|v| v.as_string())
        .ok_or("block is missing a string `name`")?
        .as_str()
        .to_string();
    let nesting = match bo
        .get("nesting")
        .and_then(|v| v.as_string())
        .map(|s| s.as_str())
    {
        Some("single") => NestingMode::Single,
        Some("set") => NestingMode::Set,
        Some("map") => NestingMode::Map,
        // The common case (a repeatable `name { … }`) and the default.
        Some("list") | None => NestingMode::List,
        Some(other) => return Err(format!("block `{name}` has unknown nesting `{other}`")),
    };
    let int = |key: &str| {
        bo.get(key)
            .and_then(|v| v.as_number())
            .and_then(|n| n.to_i64())
    };
    let inner = bo
        .get("block")
        .ok_or_else(|| format!("block `{name}` is missing a `block` schema"))?;
    let block = block_from_schema_value(inner)?;
    Ok(NestedBlock {
        name,
        nesting,
        block,
        min_items: int("minItems").unwrap_or(0),
        max_items: int("maxItems").unwrap_or(0),
    })
}

/// Decode a `cty` type constraint (itself JSON) by re-serializing it and reusing
/// the canonical cty decoder — the same trick `block_from_schema_value` uses.
fn cty_type_from(value: &facet_value::Value) -> std::result::Result<Type, String> {
    let bytes = facet_json::to_string(value).map_err(|e| e.to_string())?;
    Type::from_cty_json_bytes(bytes.as_bytes())
}

/// Build a [`FunctionSignature`] from the JS description:
/// `{ "params": [ { "name", "type": <cty>, "allowNull"?, "description"? } ],
///    "variadic"?: { … same … }, "return": <cty>, "summary"?, "description"? }`.
fn function_signature_from_json(
    name: String,
    json: &str,
) -> std::result::Result<FunctionSignature, String> {
    let parsed: facet_value::Value =
        facet_json::from_slice(json.as_bytes()).map_err(|e| e.to_string())?;
    let obj = parsed.as_object().ok_or("function signature must be an object")?;

    let parse_param = |p: &facet_value::Value| -> std::result::Result<Parameter, String> {
        let po = p.as_object().ok_or("each parameter must be an object")?;
        let pname = po
            .get("name")
            .and_then(|v| v.as_string())
            .map(|s| s.as_str().to_string())
            .unwrap_or_default();
        let ty = cty_type_from(
            po.get("type")
                .ok_or_else(|| format!("parameter `{pname}` is missing a `type`"))?,
        )?;
        Ok(Parameter {
            name: pname,
            ty,
            allow_null: po.get("allowNull").and_then(|v| v.as_bool()).unwrap_or(false),
            // Functions over an unknown argument default to an unknown result.
            allow_unknown: false,
            description: po
                .get("description")
                .and_then(|v| v.as_string())
                .map(|s| s.as_str().to_string())
                .unwrap_or_default(),
        })
    };

    let parameters = match obj.get("params").and_then(|v| v.as_array()) {
        Some(ps) => ps
            .iter()
            .map(&parse_param)
            .collect::<std::result::Result<Vec<_>, String>>()?,
        None => Vec::new(),
    };
    let variadic = obj.get("variadic").map(&parse_param).transpose()?;
    let return_type = cty_type_from(
        obj.get("return")
            .ok_or("function signature is missing a `return` type")?,
    )?;
    let string_field = |key: &str| {
        obj.get(key)
            .and_then(|v| v.as_string())
            .map(|s| s.as_str().to_string())
            .unwrap_or_default()
    };
    Ok(FunctionSignature {
        name,
        parameters,
        variadic,
        return_type,
        summary: string_field("summary"),
        description: string_field("description"),
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

/// Invoke a JS handler with the ambient handler [`Ctx`] threaded across the
/// boundary. The handler receives
/// `{ "ctx": { "private": <string|null>, "cancelled": <bool> }, "value": <value> }`
/// and returns `{ "value": <result>, "ctx"?: { "private"?: <string>,
/// "warnings"?: [ … ] } }`. Incoming private state and cancellation come from the
/// ambient `Ctx`; the handler's new private state and success-path warnings are
/// written back to it (the service reads them after the dispatch). The call is
/// raced against cancellation so `StopProvider` aborts the dispatch promptly,
/// matching the Rust `Ctx::cancelled` semantics. `value_json` is the handler's
/// payload, already serialized; the returned string is the extracted `value`.
async fn call_with_ctx(
    handler: &Handler,
    value_json: String,
    op: &str,
) -> std::result::Result<String, Diagnostics> {
    let ctx = current_ctx();
    let private = ctx.private();
    let private_json = if private.is_empty() {
        "null".to_string()
    } else {
        facet_json::to_string(&String::from_utf8_lossy(private).into_owned())
            .map_err(|e| handler_err(op, e))?
    };
    let cancelled = ctx.is_cancelled();
    let input =
        format!(r#"{{"ctx":{{"private":{private_json},"cancelled":{cancelled}}},"value":{value_json}}}"#);

    // Race the JS call against cancellation so `StopProvider` aborts the dispatch
    // promptly (the JS handler keeps running but its result is abandoned),
    // matching the Rust `Ctx::cancelled` semantics.
    let cancel = ctx.cancellation();
    let call = async {
        let promise = handler.call_async(Ok(input)).await.map_err(|e| handler_err(op, e))?;
        promise.await.map_err(|e| handler_err(op, e))
    };
    let out = match cancel.run_until_cancelled(call).await {
        Some(res) => res?,
        None => {
            return Err(vec![Diag::error(
                format!("{op} cancelled"),
                "the provider received StopProvider".to_string(),
            )]);
        }
    };

    // Drain the handler's ctx side effects into the ambient context, then return
    // its `value`. Clones of `current_ctx()` share the same sink, so these writes
    // are read back by the service.
    let parsed: facet_value::Value =
        facet_json::from_slice(out.as_bytes()).map_err(|e| handler_err(op, e))?;
    let obj = parsed
        .as_object()
        .ok_or_else(|| handler_err(op, "handler output must be a `{ value, ctx }` object"))?;
    let mut sink = current_ctx();
    if let Some(ctx_obj) = obj.get("ctx").and_then(|v| v.as_object()) {
        if let Some(p) = ctx_obj.get("private").and_then(|v| v.as_string()) {
            sink.set_private(p.as_str().as_bytes().to_vec());
        }
        if let Some(ws) = ctx_obj.get("warnings").filter(|v| v.as_array().is_some()) {
            let ws_json = facet_json::to_string(ws).map_err(|e| handler_err(op, e))?;
            for diag in parse_diagnostics(&ws_json) {
                sink.warning(diag);
            }
        }
    }
    let value = obj
        .get("value")
        .ok_or_else(|| handler_err(op, "handler output is missing `value`"))?;
    facet_json::to_string(value).map_err(|e| handler_err(op, e))
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

/// Serialize a [`Value`] for a ctx-threaded handler call, mapping the codec error
/// into diagnostics.
fn value_json(value: &Value, op: &str) -> std::result::Result<String, Diagnostics> {
    value_to_json(value).map_err(|e| handler_err(op, e))
}

#[async_trait]
impl DynResource for JsResource {
    async fn create(&self, planned: Value) -> std::result::Result<Value, Diagnostics> {
        let out = call_with_ctx(&self.create, value_json(&planned, "create")?, "create").await?;
        json_to_value(&out, &self.ty).map_err(|e| handler_err("create", e))
    }

    async fn read(&self, current: Value) -> std::result::Result<Option<Value>, Diagnostics> {
        let out = call_with_ctx(&self.read, value_json(&current, "read")?, "read").await?;
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
        let out = call_with_ctx(&self.update, value_json(&input, "update")?, "update").await?;
        json_to_value(&out, &self.ty).map_err(|e| handler_err("update", e))
    }

    async fn delete(&self, prior: Value) -> std::result::Result<(), Diagnostics> {
        call_with_ctx(&self.delete, value_json(&prior, "delete")?, "delete").await?;
        Ok(())
    }

    async fn import(&self, id: String) -> std::result::Result<Value, Diagnostics> {
        // The import handler's payload is the raw ID string; wrap it as the ctx
        // envelope's `value` (a JSON string).
        let id_json = facet_json::to_string(&id).map_err(|e| handler_err("import", e))?;
        let out = call_with_ctx(&self.import, id_json, "import").await?;
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
        let out = call_with_ctx(&self.upgrade, value_json(&input, "upgrade")?, "upgrade").await?;
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
        let out = call_with_ctx(&self.read, value_json(&config, "data source read")?, "data source read")
            .await?;
        json_to_value(&out, &self.ty).map_err(|e| handler_err("data source read", e))
    }

    async fn validate(&self, config: Value) -> Diagnostics {
        match call_handler(&self.validate, &config, "data source validate").await {
            Ok(out) => parse_diagnostics(&out),
            Err(diags) => diags,
        }
    }
}

/// An ephemeral resource whose `Open`/`Renew`/`Close`/`validate` lifecycle is a
/// set of async JS callbacks.
///
/// `Renew`/`Close` receive only the private handle (the protocol gives them
/// nothing else), so `open` returns `{ result, private?, renewAt? }`: the private
/// string and renewal deadline are written to the ambient [`Ctx`] (via
/// `current_ctx`), which the service reads back. The handle reaches `renew`/`close`
/// as the (UTF-8) private string the JS author stashed.
struct JsEphemeral {
    /// The ephemeral resource's cty object type, used to type the `open` result.
    ty: Type,
    open: Handler,
    renew: Handler,
    close: Handler,
    validate: Handler,
}

/// Apply a handler's `{ private?, renewAt? }` JSON to the ambient context: the
/// private handle (an opaque string, stored as UTF-8 bytes) and the renewal
/// deadline (`renewAt`, milliseconds since the Unix epoch).
fn apply_open_outputs(obj: &facet_value::Value) {
    let mut ctx = current_ctx();
    if let Some(p) = obj
        .as_object()
        .and_then(|o| o.get("private"))
        .and_then(|v| v.as_string())
    {
        ctx.set_private(p.as_str().as_bytes().to_vec());
    }
    if let Some(ms) = obj
        .as_object()
        .and_then(|o| o.get("renewAt"))
        .and_then(|v| v.as_number())
        .and_then(|n| n.to_i64())
        .filter(|ms| *ms >= 0)
    {
        ctx.set_renew_at(SystemTime::UNIX_EPOCH + Duration::from_millis(ms as u64));
    }
}

/// Call a JS handler whose input is the raw private handle string (not a
/// marshalled `Value`), awaiting its JSON result.
async fn call_with_handle(
    handler: &Handler,
    ctx: &str,
) -> std::result::Result<String, Diagnostics> {
    let handle = String::from_utf8_lossy(current_ctx().private()).into_owned();
    let promise = handler
        .call_async(Ok(handle))
        .await
        .map_err(|e| handler_err(ctx, e))?;
    promise.await.map_err(|e| handler_err(ctx, e))
}

#[async_trait]
impl DynEphemeral for JsEphemeral {
    async fn open(&self, config: Value) -> std::result::Result<Value, Diagnostics> {
        let out = call_handler(&self.open, &config, "open").await?;
        let parsed: facet_value::Value =
            facet_json::from_slice(out.as_bytes()).map_err(|e| handler_err("open", e))?;
        let result = parsed
            .as_object()
            .and_then(|o| o.get("result"))
            .ok_or_else(|| {
                handler_err("open", "open must return `{ result, private?, renewAt? }`")
            })?;
        // Stash the private handle / renewal deadline on the ambient ctx.
        apply_open_outputs(&parsed);
        // The result is itself a Value (typed by the schema); re-serialize and
        // decode it under the ephemeral type.
        let result_json = facet_json::to_string(result).map_err(|e| handler_err("open", e))?;
        json_to_value(&result_json, &self.ty).map_err(|e| handler_err("open", e))
    }

    async fn renew(&self) -> std::result::Result<(), Diagnostics> {
        let out = call_with_handle(&self.renew, "renew").await?;
        // A `null` (or non-object) result means "no change"; otherwise apply any
        // refreshed private/renewAt.
        if let Ok(parsed) = facet_json::from_slice::<facet_value::Value>(out.as_bytes()) {
            if parsed.as_object().is_some() {
                apply_open_outputs(&parsed);
            }
        }
        Ok(())
    }

    async fn close(&self) -> std::result::Result<(), Diagnostics> {
        call_with_handle(&self.close, "close").await?;
        Ok(())
    }

    async fn validate(&self, config: Value) -> Diagnostics {
        match call_handler(&self.validate, &config, "ephemeral validate").await {
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

/// A provider-config `validate` callback: one async JS handler returning a
/// diagnostics array. Runs in `ValidateProviderConfig`, before `configure`.
struct JsValidateConfig {
    validate: Handler,
}

#[async_trait]
impl DynValidateConfig for JsValidateConfig {
    async fn validate(&self, config: Value) -> Diagnostics {
        match call_handler(&self.validate, &config, "provider validate").await {
            Ok(out) => parse_diagnostics(&out),
            Err(diags) => diags,
        }
    }
}

/// A provider-defined function: one async JS handler over positional arguments.
/// The service has already decoded each argument under its parameter's cty type;
/// they arrive as a `Vec<Value>` and are marshalled to JS as a JSON array.
struct JsFunction {
    /// The function's cty return type, used to type the handler's result.
    return_ty: Type,
    call: Handler,
}

#[async_trait]
impl DynFunction for JsFunction {
    async fn call(&self, args: Vec<Value>) -> std::result::Result<Value, FunctionError> {
        let jsons = args
            .iter()
            .map(value_to_json)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(FunctionError::new)?;
        let input = format!("[{}]", jsons.join(","));
        let promise = self
            .call
            .call_async(Ok(input))
            .await
            .map_err(|e| FunctionError::new(e.to_string()))?;
        let out = promise.await.map_err(|e| FunctionError::new(e.to_string()))?;
        json_to_value(&out, &self.return_ty).map_err(FunctionError::new)
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

/// A registered ephemeral resource.
struct EphemeralReg {
    name: String,
    block: Block,
    handler: Arc<dyn DynEphemeral>,
}

/// A registered provider-defined function.
struct FunctionReg {
    signature: FunctionSignature,
    handler: Arc<dyn DynFunction>,
}

/// The provider-level config block, its configure handler, and an optional
/// validate handler (run before configure).
struct ProviderConfigReg {
    block: Block,
    handler: Arc<dyn DynConfigure>,
    validate: Arc<dyn DynValidateConfig>,
}

/// The provider definition assembled from JS, then served over tfplugin6.
#[napi]
pub struct Provider {
    resources: Vec<ResourceReg>,
    data_sources: Vec<DataSourceReg>,
    ephemerals: Vec<EphemeralReg>,
    functions: Vec<FunctionReg>,
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
            ephemerals: Vec::new(),
            functions: Vec::new(),
            config: None,
        }
    }

    /// Declare the provider's configuration block, the async `configure` handler
    /// (run on `ConfigureProvider`), and a `validate` handler (run before it, in
    /// `ValidateProviderConfig`). The configure handler typically stores state the
    /// resource/data-source handlers close over; validate returns a diagnostics
    /// array for a bad provider block.
    #[napi]
    pub fn config(
        &mut self,
        schema_json: String,
        configure: ThreadsafeFunction<String, Promise<String>>,
        validate: ThreadsafeFunction<String, Promise<String>>,
    ) -> Result<()> {
        let block = block_from_schema_json(&schema_json).map_err(napi_err)?;
        self.config = Some(ProviderConfigReg {
            block,
            handler: Arc::new(JsConfigure { configure }),
            validate: Arc::new(JsValidateConfig { validate }),
        });
        Ok(())
    }

    /// Register a provider-defined function: its `name`, a signature description
    /// (`signature_json`: ordered `params`, an optional `variadic` parameter, and
    /// the `return` type, all cty-typed), and a single async `call` handler that
    /// receives the positional arguments as a JSON array and returns the result.
    #[napi]
    pub fn function(
        &mut self,
        name: String,
        signature_json: String,
        call: ThreadsafeFunction<String, Promise<String>>,
    ) -> Result<()> {
        let signature = function_signature_from_json(name, &signature_json).map_err(napi_err)?;
        let return_ty = signature.return_type.clone();
        self.functions.push(FunctionReg {
            signature,
            handler: Arc::new(JsFunction { return_ty, call }),
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

    /// Register an ephemeral resource: its `type_name`, a schema description, and
    /// the async lifecycle handlers. `open` receives the marshalled config and
    /// returns `{ result, private?, renewAt? }` JSON; `renew`/`close` receive the
    /// raw private handle string; `validate` receives the config and returns a
    /// diagnostics array.
    #[napi]
    pub fn ephemeral(
        &mut self,
        type_name: String,
        schema_json: String,
        open: ThreadsafeFunction<String, Promise<String>>,
        renew: ThreadsafeFunction<String, Promise<String>>,
        close: ThreadsafeFunction<String, Promise<String>>,
        validate: ThreadsafeFunction<String, Promise<String>>,
    ) -> Result<()> {
        let block = block_from_schema_json(&schema_json).map_err(napi_err)?;
        let ty = block.cty_type();
        self.ephemerals.push(EphemeralReg {
            name: type_name,
            block,
            handler: Arc::new(JsEphemeral {
                ty,
                open,
                renew,
                close,
                validate,
            }),
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
                .dyn_configure(Arc::clone(&cfg.handler))
                .dyn_validate_config(Arc::clone(&cfg.validate));
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
        for e in &self.ephemerals {
            builder =
                builder.dyn_ephemeral(e.name.clone(), e.block.clone(), Arc::clone(&e.handler));
        }
        for f in &self.functions {
            builder = builder.dyn_function(f.signature.clone(), Arc::clone(&f.handler));
        }
        let provider = builder.build().map_err(napi_err)?;
        serve_provider(provider).await.map_err(napi_err)
    }
}
