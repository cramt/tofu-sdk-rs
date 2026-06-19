//! The gRPC [`tfplugin6::provider_server::Provider`] implementation.
//!
//! Answers schema discovery (`GetMetadata`, `GetProviderSchema`), the resource
//! lifecycle (`ConfigureProvider`, validation, `UpgradeResourceState`,
//! `PlanResourceChange`, `ReadResource`, `ApplyResourceChange`), and data source
//! reads (`ReadDataSource`), the ephemeral resource lifecycle
//! (`Open`/`Renew`/`Close`/`ValidateEphemeralResourceConfig`), import
//! (`ImportResourceState`), and provider-defined functions
//! (`GetFunctions`/`CallFunction`) by dispatching to the registered handlers
//! through the value codec. RPCs for features the SDK does not yet support
//! (identity, state stores, actions, move) still return `Unimplemented`.

use std::pin::Pin;

use terraform_codec::{decode_json, decode_json_value, decode_msgpack, encode_msgpack, CodecError};
use terraform_tfplugin6::{emit_functions, emit_metadata, emit_provider_schema, tfplugin6};
use terraform_value::{Type, Value};
use tokio_util::sync::CancellationToken;
use tonic::codegen::tokio_stream::Stream;
use tonic::{Request, Response, Status};

use crate::builder::Provider;
use crate::ctx::{with_ctx, Ctx, CtxOutputs};
use crate::plan::plan;
use crate::resource::{Diag, Diagnostics, Severity};

/// A boxed server-streaming response of `T`. Used only to satisfy the trait's
/// associated stream types for the not-yet-implemented streaming RPCs.
type BoxStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

tokio::task_local! {
    /// The cancellation token for the in-flight RPC's handler, scoped around each
    /// dispatch so handlers can observe `StopProvider` via [`current_cancellation`].
    static CANCEL: CancellationToken;
}

/// The cancellation token for the currently-executing handler, if called from
/// within the runtime's dispatch. A handler can `select!` on
/// [`CancellationToken::cancelled`] to abort promptly when Terraform sends
/// `StopProvider`. Returns `None` when called outside a dispatch (e.g. a unit
/// test invoking a handler directly).
pub fn current_cancellation() -> Option<CancellationToken> {
    CANCEL.try_with(|token| token.clone()).ok()
}

/// Adapts a [`Provider`] to the generated gRPC service trait.
#[derive(Clone)]
pub struct ProviderService {
    provider: Provider,
    /// Tripped by `StopProvider`; cloned into each handler dispatch's task-local.
    cancel: CancellationToken,
}

impl ProviderService {
    /// Wrap a built provider.
    pub fn new(provider: Provider) -> Self {
        Self {
            provider,
            cancel: CancellationToken::new(),
        }
    }

    /// Run an erased handler future under the handler [`Ctx`] (carrying
    /// `private_in`) and the cancellation scope, converting a panic into an error
    /// diagnostic so a buggy handler yields a clean failure instead of unwinding
    /// out of the async task and tearing down the plugin process (requires the
    /// default `panic = "unwind"`). Returns the handler result together with the
    /// context's accumulated outputs (success-path warnings, new private state).
    async fn run<T>(
        &self,
        op: &str,
        private_in: Vec<u8>,
        fut: impl std::future::Future<Output = Result<T, Diagnostics>>,
    ) -> (Result<T, Diagnostics>, CtxOutputs) {
        let ctx = Ctx::new(private_in, self.cancel.clone());
        with_ctx(ctx, self.catch(op, fut)).await
    }

    /// Like [`run`](Self::run), for a handler returning bare [`Diagnostics`]
    /// (validation); there is no private state.
    async fn run_diags(
        &self,
        op: &str,
        fut: impl std::future::Future<Output = Diagnostics>,
    ) -> (Diagnostics, CtxOutputs) {
        let ctx = Ctx::new(Vec::new(), self.cancel.clone());
        with_ctx(ctx, self.catch_diags(op, fut)).await
    }

    /// Scope the cancellation token around `fut` and turn a handler panic into an
    /// error diagnostic.
    async fn catch<T>(
        &self,
        op: &str,
        fut: impl std::future::Future<Output = Result<T, Diagnostics>>,
    ) -> Result<T, Diagnostics> {
        use futures_util::future::FutureExt;
        let scoped = CANCEL.scope(self.cancel.clone(), fut);
        match std::panic::AssertUnwindSafe(scoped).catch_unwind().await {
            Ok(res) => res,
            Err(payload) => Err(vec![Diag::error(
                format!("the {op} handler panicked"),
                panic_message(payload),
            )]),
        }
    }

    /// [`catch`](Self::catch) for a handler returning bare [`Diagnostics`].
    async fn catch_diags(
        &self,
        op: &str,
        fut: impl std::future::Future<Output = Diagnostics>,
    ) -> Diagnostics {
        use futures_util::future::FutureExt;
        let scoped = CANCEL.scope(self.cancel.clone(), fut);
        match std::panic::AssertUnwindSafe(scoped).catch_unwind().await {
            Ok(diags) => diags,
            Err(payload) => vec![Diag::error(
                format!("the {op} handler panicked"),
                panic_message(payload),
            )],
        }
    }
}

/// Generate `Unimplemented` stubs for the unary RPCs not yet supported.
///
/// The bodies are emitted in the already-desugared `#[async_trait]` form
/// (returning a boxed future) rather than as `async fn`. This is deliberate: the
/// `#[async_trait]` attribute on the impl runs *before* this function-like macro
/// expands, so it would never rewrite `async fn`s produced here. Emitting the
/// final signature ourselves sidesteps that macro-ordering issue, and it must
/// match the trait's desugared signature exactly.
macro_rules! unimplemented_unary {
    ($($method:ident => $module:ident),* $(,)?) => {
        $(
            fn $method<'life0, 'async_trait>(
                &'life0 self,
                _request: Request<tfplugin6::$module::Request>,
            ) -> ::core::pin::Pin<
                Box<
                    dyn ::core::future::Future<
                            Output = Result<Response<tfplugin6::$module::Response>, Status>,
                        > + ::core::marker::Send
                        + 'async_trait,
                >,
            >
            where
                'life0: 'async_trait,
                Self: 'async_trait,
            {
                Box::pin(async move {
                    Err(Status::unimplemented(concat!(
                        stringify!($method),
                        " is not implemented yet"
                    )))
                })
            }
        )*
    };
}

#[tonic::async_trait]
impl tfplugin6::provider_server::Provider for ProviderService {
    async fn get_metadata(
        &self,
        _request: Request<tfplugin6::get_metadata::Request>,
    ) -> Result<Response<tfplugin6::get_metadata::Response>, Status> {
        Ok(Response::new(emit_metadata(self.provider.schema())))
    }

    async fn get_provider_schema(
        &self,
        _request: Request<tfplugin6::get_provider_schema::Request>,
    ) -> Result<Response<tfplugin6::get_provider_schema::Response>, Status> {
        Ok(Response::new(emit_provider_schema(self.provider.schema())))
    }

    // --- Provider configuration & validation -------------------------------

    async fn configure_provider(
        &self,
        request: Request<tfplugin6::configure_provider::Request>,
    ) -> Result<Response<tfplugin6::configure_provider::Response>, Status> {
        use tfplugin6::configure_provider::Response as Resp;
        let req = request.into_inner();
        tracing::debug!("ConfigureProvider");

        let ty = self.provider.provider_config_ty();
        let config = match decode_dynamic(&req.config, &ty) {
            Ok(v) => v,
            Err(e) => {
                return Ok(Response::new(Resp {
                    diagnostics: error_diag("failed to decode provider config", e.to_string()),
                }))
            }
        };

        match self.provider.configure(config).await {
            Ok(()) => Ok(Response::new(Default::default())),
            Err(diags) => Ok(Response::new(Resp {
                diagnostics: pb_diagnostics(diags),
            })),
        }
    }

    async fn validate_provider_config(
        &self,
        request: Request<tfplugin6::validate_provider_config::Request>,
    ) -> Result<Response<tfplugin6::validate_provider_config::Response>, Status> {
        use tfplugin6::validate_provider_config::Response as Resp;
        let req = request.into_inner();

        let ty = self.provider.provider_config_ty();
        let config = match decode_dynamic(&req.config, &ty) {
            Ok(v) => v,
            Err(e) => {
                return Ok(Response::new(Resp {
                    diagnostics: error_diag("failed to decode provider config", e.to_string()),
                }))
            }
        };
        Ok(Response::new(Resp {
            diagnostics: pb_diagnostics(self.provider.validate_config(config).await),
        }))
    }

    async fn validate_resource_config(
        &self,
        request: Request<tfplugin6::validate_resource_config::Request>,
    ) -> Result<Response<tfplugin6::validate_resource_config::Response>, Status> {
        use tfplugin6::validate_resource_config::Response as Resp;
        let req = request.into_inner();

        let Some(ty) = self.provider.resource_cty(&req.type_name) else {
            return Ok(Response::new(Resp {
                diagnostics: unknown_resource(&req.type_name),
            }));
        };
        // ValidateResourceConfig runs before ConfigureProvider, so a meta-backed
        // handler may not exist yet. The resource is still known (it's in the
        // schema), so skip validation rather than erroring.
        let Some(handler) = self.provider.resource_handler(&req.type_name) else {
            return Ok(Response::new(Resp::default()));
        };
        let config = match decode_dynamic(&req.config, &ty) {
            Ok(v) => v,
            Err(e) => {
                return Ok(Response::new(Resp {
                    diagnostics: error_diag("failed to decode config", e.to_string()),
                }))
            }
        };
        let (diags, outs) = self
            .run_diags("validate resource config", handler.validate(config))
            .await;
        Ok(Response::new(Resp {
            diagnostics: diags_with_warnings(diags, outs),
        }))
    }

    async fn validate_data_resource_config(
        &self,
        request: Request<tfplugin6::validate_data_resource_config::Request>,
    ) -> Result<Response<tfplugin6::validate_data_resource_config::Response>, Status> {
        use tfplugin6::validate_data_resource_config::Response as Resp;
        let req = request.into_inner();

        let Some(ty) = self.provider.data_source_cty(&req.type_name) else {
            return Ok(Response::new(Resp {
                diagnostics: unknown_data_source(&req.type_name),
            }));
        };
        // As with resources, the handler may not be built yet (runs before
        // ConfigureProvider); skip validation rather than erroring.
        let Some(handler) = self.provider.data_source_handler(&req.type_name) else {
            return Ok(Response::new(Resp::default()));
        };
        let config = match decode_dynamic(&req.config, &ty) {
            Ok(v) => v,
            Err(e) => {
                return Ok(Response::new(Resp {
                    diagnostics: error_diag("failed to decode config", e.to_string()),
                }))
            }
        };
        let (diags, outs) = self
            .run_diags("validate data source config", handler.validate(config))
            .await;
        Ok(Response::new(Resp {
            diagnostics: diags_with_warnings(diags, outs),
        }))
    }

    // --- Resource lifecycle ------------------------------------------------

    async fn upgrade_resource_state(
        &self,
        request: Request<tfplugin6::upgrade_resource_state::Request>,
    ) -> Result<Response<tfplugin6::upgrade_resource_state::Response>, Status> {
        use tfplugin6::upgrade_resource_state::Response as Resp;
        let req = request.into_inner();

        let (Some(ty), Some(current_version)) = (
            self.provider.resource_cty(&req.type_name),
            self.provider.resource_version(&req.type_name),
        ) else {
            return Ok(Response::new(Resp {
                diagnostics: unknown_resource(&req.type_name),
                ..Default::default()
            }));
        };

        // Stored state arrives as cty JSON in `raw_state.json`.
        let raw = match req.raw_state.as_ref() {
            Some(raw) if !raw.json.is_empty() => raw.json.as_slice(),
            // Empty state (a fresh resource) needs no upgrade.
            _ => {
                return Ok(Response::new(Resp {
                    upgraded_state: encode_dynamic(&Value::Null, &ty).ok(),
                    ..Default::default()
                }))
            }
        };
        let json: facet_value::Value = match facet_json::from_slice(raw) {
            Ok(j) => j,
            Err(e) => {
                return Ok(Response::new(Resp {
                    diagnostics: error_diag("failed to parse prior state", e.to_string()),
                    ..Default::default()
                }))
            }
        };

        // Up to date already: the stored state matches the current schema, so it
        // decodes directly. Otherwise hand the untyped prior state to the
        // resource's upgrade migration.
        let mut warnings: Diagnostics = Vec::new();
        let upgraded = if req.version >= current_version {
            decode_json(&json, &ty).map_err(|e| e.to_string())
        } else {
            let Some(handler) = self.provider.resource_handler(&req.type_name) else {
                return Ok(Response::new(Resp {
                    diagnostics: unknown_resource(&req.type_name),
                    ..Default::default()
                }));
            };
            let prior = match decode_json_value(&json) {
                Ok(v) => v,
                Err(e) => {
                    return Ok(Response::new(Resp {
                        diagnostics: error_diag("failed to read prior state", e.to_string()),
                        ..Default::default()
                    }))
                }
            };
            let (outcome, outs) = self
                .run(
                    "upgrade resource state",
                    Vec::new(),
                    handler.upgrade(req.version, prior),
                )
                .await;
            match outcome {
                Ok(v) => {
                    warnings = outs.warnings;
                    Ok(v)
                }
                Err(diags) => {
                    return Ok(Response::new(Resp {
                        diagnostics: diags_with_warnings(diags, outs),
                        ..Default::default()
                    }))
                }
            }
        };

        match upgraded.and_then(|v| encode_dynamic(&v, &ty).map_err(|e| e.to_string())) {
            Ok(dv) => Ok(Response::new(Resp {
                upgraded_state: Some(dv),
                diagnostics: pb_diagnostics(warnings),
            })),
            Err(e) => {
                let mut diagnostics = pb_diagnostics(warnings);
                diagnostics.extend(error_diag("failed to upgrade state", e));
                Ok(Response::new(Resp {
                    diagnostics,
                    ..Default::default()
                }))
            }
        }
    }

    async fn plan_resource_change(
        &self,
        request: Request<tfplugin6::plan_resource_change::Request>,
    ) -> Result<Response<tfplugin6::plan_resource_change::Response>, Status> {
        use tfplugin6::plan_resource_change::Response as Resp;
        let req = request.into_inner();
        tracing::debug!(type_name = %req.type_name, "PlanResourceChange");

        let (Some(ty), Some(block)) = (
            self.provider.resource_cty(&req.type_name),
            self.provider.resource_block(&req.type_name),
        ) else {
            return Ok(Response::new(Resp {
                diagnostics: unknown_resource(&req.type_name),
                ..Default::default()
            }));
        };

        let prior = match decode_dynamic(&req.prior_state, &ty) {
            Ok(v) => v,
            Err(e) => {
                return Ok(Response::new(Resp {
                    diagnostics: error_diag("failed to decode prior state", e.to_string()),
                    ..Default::default()
                }))
            }
        };
        let proposed = match decode_dynamic(&req.proposed_new_state, &ty) {
            Ok(v) => v,
            Err(e) => {
                return Ok(Response::new(Resp {
                    diagnostics: error_diag("failed to decode proposed state", e.to_string()),
                    ..Default::default()
                }))
            }
        };

        let mut plan = plan(&prior, proposed, block);

        // Author plan modification: adjust the mechanical plan (force-replace by
        // rule, mark computed-by-rule unknown). Skipped when no handler is built
        // yet (a meta-backed handler exists only after ConfigureProvider).
        let mut warnings: Diagnostics = Vec::new();
        let mut planned_private = req.planned_private;
        if let Some(handler) = self.provider.resource_handler(&req.type_name) {
            let (outcome, outs) = self
                .run(
                    "modify_plan",
                    planned_private.clone(),
                    handler.modify_plan(prior, plan.planned.clone()),
                )
                .await;
            match outcome {
                Ok(mods) => {
                    crate::plan::apply_modifications(&mut plan, mods);
                    let CtxOutputs {
                        warnings: w,
                        private_out,
                        ..
                    } = outs;
                    warnings = w;
                    if let Some(p) = private_out {
                        planned_private = p;
                    }
                }
                Err(diags) => {
                    return Ok(Response::new(Resp {
                        diagnostics: diags_with_warnings(diags, outs),
                        ..Default::default()
                    }))
                }
            }
        }

        match encode_dynamic(&plan.planned, &ty) {
            Ok(dv) => Ok(Response::new(Resp {
                planned_state: Some(dv),
                requires_replace: plan.requires_replace,
                planned_private,
                diagnostics: pb_diagnostics(warnings),
                ..Default::default()
            })),
            Err(e) => {
                let mut diagnostics = pb_diagnostics(warnings);
                diagnostics.extend(error_diag("failed to encode planned state", e.to_string()));
                Ok(Response::new(Resp {
                    diagnostics,
                    ..Default::default()
                }))
            }
        }
    }

    async fn read_resource(
        &self,
        request: Request<tfplugin6::read_resource::Request>,
    ) -> Result<Response<tfplugin6::read_resource::Response>, Status> {
        use tfplugin6::read_resource::Response as Resp;
        let req = request.into_inner();
        tracing::debug!(type_name = %req.type_name, "ReadResource");

        let (Some(ty), Some(handler)) = (
            self.provider.resource_cty(&req.type_name),
            self.provider.resource_handler(&req.type_name),
        ) else {
            return Ok(Response::new(Resp {
                diagnostics: unknown_resource(&req.type_name),
                ..Default::default()
            }));
        };

        let current = match decode_dynamic(&req.current_state, &ty) {
            Ok(v) => v,
            Err(e) => {
                return Ok(Response::new(Resp {
                    diagnostics: error_diag("failed to decode current state", e.to_string()),
                    ..Default::default()
                }))
            }
        };

        let (outcome, outs) = self
            .run("read resource", req.private.clone(), handler.read(current))
            .await;
        let new_value = match outcome {
            Ok(Some(v)) => v,
            Ok(None) => Value::Null, // resource is gone
            Err(diags) => {
                return Ok(Response::new(Resp {
                    diagnostics: diags_with_warnings(diags, outs),
                    ..Default::default()
                }))
            }
        };

        respond_state(
            &new_value,
            &ty,
            req.private,
            outs,
            |new_state, private, diagnostics| Resp {
                new_state,
                private,
                diagnostics,
                ..Default::default()
            },
        )
    }

    async fn apply_resource_change(
        &self,
        request: Request<tfplugin6::apply_resource_change::Request>,
    ) -> Result<Response<tfplugin6::apply_resource_change::Response>, Status> {
        use tfplugin6::apply_resource_change::Response as Resp;
        let req = request.into_inner();

        let (Some(ty), Some(handler)) = (
            self.provider.resource_cty(&req.type_name),
            self.provider.resource_handler(&req.type_name),
        ) else {
            return Ok(Response::new(Resp {
                diagnostics: unknown_resource(&req.type_name),
                ..Default::default()
            }));
        };

        let prior = match decode_dynamic(&req.prior_state, &ty) {
            Ok(v) => v,
            Err(e) => {
                return Ok(Response::new(Resp {
                    diagnostics: error_diag("failed to decode prior state", e.to_string()),
                    ..Default::default()
                }))
            }
        };
        let planned = match decode_dynamic(&req.planned_state, &ty) {
            Ok(v) => v,
            Err(e) => {
                return Ok(Response::new(Resp {
                    diagnostics: error_diag("failed to decode planned state", e.to_string()),
                    ..Default::default()
                }))
            }
        };

        // null planned => destroy; null prior => create; otherwise update.
        let action = if planned.is_null() {
            "delete"
        } else if prior.is_null() {
            "create"
        } else {
            "update"
        };
        tracing::debug!(type_name = %req.type_name, action, "ApplyResourceChange");
        let private_in = req.planned_private.clone();
        let (outcome, outs): (Result<Value, Diagnostics>, CtxOutputs) = if planned.is_null() {
            self.run("delete", private_in, async {
                handler.delete(prior.clone()).await.map(|()| Value::Null)
            })
            .await
        } else if prior.is_null() {
            self.run("create", private_in, handler.create(planned))
                .await
        } else {
            self.run("update", private_in, handler.update(planned, prior.clone()))
                .await
        };

        match outcome {
            Ok(new_value) => respond_state(
                &new_value,
                &ty,
                req.planned_private,
                outs,
                |new_state, private, diagnostics| Resp {
                    new_state,
                    private,
                    diagnostics,
                    ..Default::default()
                },
            ),
            Err(diags) => Ok(Response::new(Resp {
                // On failure, keep the prior state so Terraform does not record a
                // partially-applied resource.
                new_state: encode_dynamic(&prior, &ty).ok(),
                diagnostics: diags_with_warnings(diags, outs),
                ..Default::default()
            })),
        }
    }

    async fn read_data_source(
        &self,
        request: Request<tfplugin6::read_data_source::Request>,
    ) -> Result<Response<tfplugin6::read_data_source::Response>, Status> {
        use tfplugin6::read_data_source::Response as Resp;
        let req = request.into_inner();
        tracing::debug!(type_name = %req.type_name, "ReadDataSource");

        let (Some(ty), Some(handler)) = (
            self.provider.data_source_cty(&req.type_name),
            self.provider.data_source_handler(&req.type_name),
        ) else {
            return Ok(Response::new(Resp {
                diagnostics: unknown_data_source(&req.type_name),
                ..Default::default()
            }));
        };

        let config = match decode_dynamic(&req.config, &ty) {
            Ok(v) => v,
            Err(e) => {
                return Ok(Response::new(Resp {
                    diagnostics: error_diag("failed to decode data source config", e.to_string()),
                    ..Default::default()
                }))
            }
        };

        let (outcome, outs) = self
            .run("read data source", Vec::new(), handler.read(config))
            .await;
        match outcome {
            Ok(state) => match encode_dynamic(&state, &ty) {
                Ok(dv) => Ok(Response::new(Resp {
                    state: Some(dv),
                    diagnostics: pb_diagnostics(outs.warnings),
                    ..Default::default()
                })),
                Err(e) => {
                    let mut diagnostics = pb_diagnostics(outs.warnings);
                    diagnostics.extend(error_diag(
                        "failed to encode data source state",
                        e.to_string(),
                    ));
                    Ok(Response::new(Resp {
                        diagnostics,
                        ..Default::default()
                    }))
                }
            },
            Err(diags) => Ok(Response::new(Resp {
                diagnostics: diags_with_warnings(diags, outs),
                ..Default::default()
            })),
        }
    }

    // --- Ephemeral resources -----------------------------------------------

    async fn validate_ephemeral_resource_config(
        &self,
        request: Request<tfplugin6::validate_ephemeral_resource_config::Request>,
    ) -> Result<Response<tfplugin6::validate_ephemeral_resource_config::Response>, Status> {
        use tfplugin6::validate_ephemeral_resource_config::Response as Resp;
        let req = request.into_inner();

        let Some(ty) = self.provider.ephemeral_cty(&req.type_name) else {
            return Ok(Response::new(Resp {
                diagnostics: unknown_ephemeral(&req.type_name),
            }));
        };
        // Like resources/data sources, the handler may not be built yet (this runs
        // before ConfigureProvider); the type is still known, so skip rather than
        // error.
        let Some(handler) = self.provider.ephemeral_handler(&req.type_name) else {
            return Ok(Response::new(Resp::default()));
        };
        let config = match decode_dynamic(&req.config, &ty) {
            Ok(v) => v,
            Err(e) => {
                return Ok(Response::new(Resp {
                    diagnostics: error_diag("failed to decode config", e.to_string()),
                }))
            }
        };
        let (diags, outs) = self
            .run_diags("validate ephemeral config", handler.validate(config))
            .await;
        Ok(Response::new(Resp {
            diagnostics: diags_with_warnings(diags, outs),
        }))
    }

    async fn open_ephemeral_resource(
        &self,
        request: Request<tfplugin6::open_ephemeral_resource::Request>,
    ) -> Result<Response<tfplugin6::open_ephemeral_resource::Response>, Status> {
        use tfplugin6::open_ephemeral_resource::Response as Resp;
        let req = request.into_inner();
        tracing::debug!(type_name = %req.type_name, "OpenEphemeralResource");

        let (Some(ty), Some(handler)) = (
            self.provider.ephemeral_cty(&req.type_name),
            self.provider.ephemeral_handler(&req.type_name),
        ) else {
            return Ok(Response::new(Resp {
                diagnostics: unknown_ephemeral(&req.type_name),
                ..Default::default()
            }));
        };

        let config = match decode_dynamic(&req.config, &ty) {
            Ok(v) => v,
            Err(e) => {
                return Ok(Response::new(Resp {
                    diagnostics: error_diag("failed to decode ephemeral config", e.to_string()),
                    ..Default::default()
                }))
            }
        };

        // Open starts fresh — there is no prior private state.
        let (outcome, outs) = self
            .run("open ephemeral resource", Vec::new(), handler.open(config))
            .await;
        let renew_at = outs.renew_at.map(to_timestamp);
        match outcome {
            Ok(result) => match encode_dynamic(&result, &ty) {
                Ok(dv) => Ok(Response::new(Resp {
                    result: Some(dv),
                    private: outs.private_out,
                    renew_at,
                    diagnostics: pb_diagnostics(outs.warnings),
                    ..Default::default()
                })),
                Err(e) => {
                    let mut diagnostics = pb_diagnostics(outs.warnings);
                    diagnostics.extend(error_diag(
                        "failed to encode ephemeral result",
                        e.to_string(),
                    ));
                    Ok(Response::new(Resp {
                        diagnostics,
                        ..Default::default()
                    }))
                }
            },
            Err(diags) => Ok(Response::new(Resp {
                diagnostics: diags_with_warnings(diags, outs),
                ..Default::default()
            })),
        }
    }

    async fn renew_ephemeral_resource(
        &self,
        request: Request<tfplugin6::renew_ephemeral_resource::Request>,
    ) -> Result<Response<tfplugin6::renew_ephemeral_resource::Response>, Status> {
        use tfplugin6::renew_ephemeral_resource::Response as Resp;
        let req = request.into_inner();
        tracing::debug!(type_name = %req.type_name, "RenewEphemeralResource");

        let Some(handler) = self.provider.ephemeral_handler(&req.type_name) else {
            return Ok(Response::new(Resp {
                diagnostics: unknown_ephemeral(&req.type_name),
                ..Default::default()
            }));
        };

        // Renew receives only the private bytes (no config, no result).
        let incoming = req.private.clone().unwrap_or_default();
        let (outcome, outs) = self
            .run("renew ephemeral resource", incoming, handler.renew())
            .await;
        let renew_at = outs.renew_at.map(to_timestamp);
        match outcome {
            // A handler that didn't rewrite private state keeps the incoming bytes,
            // so the next renew/close still has the handle.
            Ok(()) => Ok(Response::new(Resp {
                private: outs.private_out.or(req.private),
                renew_at,
                diagnostics: pb_diagnostics(outs.warnings),
            })),
            Err(diags) => Ok(Response::new(Resp {
                diagnostics: diags_with_warnings(diags, outs),
                ..Default::default()
            })),
        }
    }

    async fn close_ephemeral_resource(
        &self,
        request: Request<tfplugin6::close_ephemeral_resource::Request>,
    ) -> Result<Response<tfplugin6::close_ephemeral_resource::Response>, Status> {
        use tfplugin6::close_ephemeral_resource::Response as Resp;
        let req = request.into_inner();
        tracing::debug!(type_name = %req.type_name, "CloseEphemeralResource");

        let Some(handler) = self.provider.ephemeral_handler(&req.type_name) else {
            return Ok(Response::new(Resp {
                diagnostics: unknown_ephemeral(&req.type_name),
            }));
        };

        let incoming = req.private.unwrap_or_default();
        let (outcome, outs) = self
            .run("close ephemeral resource", incoming, handler.close())
            .await;
        let diagnostics = match outcome {
            Ok(()) => pb_diagnostics(outs.warnings),
            Err(diags) => diags_with_warnings(diags, outs),
        };
        Ok(Response::new(Resp { diagnostics }))
    }

    async fn import_resource_state(
        &self,
        request: Request<tfplugin6::import_resource_state::Request>,
    ) -> Result<Response<tfplugin6::import_resource_state::Response>, Status> {
        use tfplugin6::import_resource_state::{ImportedResource, Response as Resp};
        let req = request.into_inner();
        tracing::debug!(type_name = %req.type_name, id = %req.id, "ImportResourceState");

        let (Some(ty), Some(handler)) = (
            self.provider.resource_cty(&req.type_name),
            self.provider.resource_handler(&req.type_name),
        ) else {
            return Ok(Response::new(Resp {
                diagnostics: unknown_resource(&req.type_name),
                ..Default::default()
            }));
        };

        // The provider produces the imported state from the ID; Terraform then
        // refreshes it with ReadResource.
        let (outcome, outs) = self
            .run("import resource state", Vec::new(), handler.import(req.id))
            .await;
        match outcome {
            Ok(state) => match encode_dynamic(&state, &ty) {
                Ok(dv) => Ok(Response::new(Resp {
                    imported_resources: vec![ImportedResource {
                        type_name: req.type_name,
                        state: Some(dv),
                        ..Default::default()
                    }],
                    diagnostics: pb_diagnostics(outs.warnings),
                    ..Default::default()
                })),
                Err(e) => {
                    let mut diagnostics = pb_diagnostics(outs.warnings);
                    diagnostics
                        .extend(error_diag("failed to encode imported state", e.to_string()));
                    Ok(Response::new(Resp {
                        diagnostics,
                        ..Default::default()
                    }))
                }
            },
            Err(diags) => Ok(Response::new(Resp {
                diagnostics: diags_with_warnings(diags, outs),
                ..Default::default()
            })),
        }
    }

    unimplemented_unary! {
        get_resource_identity_schemas => get_resource_identity_schemas,
        upgrade_resource_identity => upgrade_resource_identity,
        move_resource_state => move_resource_state,
        generate_resource_config => generate_resource_config,
        validate_list_resource_config => validate_list_resource_config,
        validate_state_store_config => validate_state_store,
        configure_state_store => configure_state_store,
        lock_state => lock_state,
        unlock_state => unlock_state,
        get_states => get_states,
        delete_state => delete_state,
        plan_action => plan_action,
        validate_action_config => validate_action_config,
    }

    // --- Provider-defined functions ----------------------------------------

    async fn get_functions(
        &self,
        _request: Request<tfplugin6::get_functions::Request>,
    ) -> Result<Response<tfplugin6::get_functions::Response>, Status> {
        tracing::debug!("GetFunctions");
        Ok(Response::new(tfplugin6::get_functions::Response {
            functions: emit_functions(self.provider.schema()),
            diagnostics: Vec::new(),
        }))
    }

    async fn call_function(
        &self,
        request: Request<tfplugin6::call_function::Request>,
    ) -> Result<Response<tfplugin6::call_function::Response>, Status> {
        use futures_util::future::FutureExt;
        use tfplugin6::call_function::Response as Resp;
        let req = request.into_inner();
        tracing::debug!(name = %req.name, "CallFunction");

        // Build a `CallFunction` error response (optionally pointed at an argument).
        let err = |text: String, argument: Option<i64>| {
            Ok(Response::new(Resp {
                result: None,
                error: Some(tfplugin6::FunctionError {
                    text,
                    function_argument: argument,
                }),
            }))
        };

        let Some(signature) = self
            .provider
            .schema()
            .functions
            .iter()
            .find(|f| f.name == req.name)
            .cloned()
        else {
            return err(
                format!("function {:?} is not defined by this provider", req.name),
                None,
            );
        };
        let Some(handler) = self.provider.function_handler(&req.name) else {
            return err(format!("function {:?} has no handler", req.name), None);
        };

        // Decode each positional argument with its parameter's `cty` type. Any
        // arguments past the fixed parameters are variadic and share the variadic
        // parameter's type; without a variadic parameter, extra arguments error.
        let mut args = Vec::with_capacity(req.arguments.len());
        for (i, dv) in req.arguments.iter().enumerate() {
            let Some(param) = signature.parameters.get(i).or(signature.variadic.as_ref()) else {
                return err(
                    format!(
                        "function {:?} received more arguments than parameters",
                        req.name
                    ),
                    Some(i as i64),
                );
            };
            match decode_argument(dv, &param.ty) {
                Ok(v) => args.push(v),
                Err(e) => {
                    return err(
                        format!("failed to decode argument {i}: {e}"),
                        Some(i as i64),
                    )
                }
            }
        }

        // Functions are pure, but still contain a panic as an error rather than
        // tearing down the plugin process.
        match std::panic::AssertUnwindSafe(handler.call(args))
            .catch_unwind()
            .await
        {
            Ok(Ok(result)) => match encode_dynamic(&result, &signature.return_type) {
                Ok(dv) => Ok(Response::new(Resp {
                    result: Some(dv),
                    error: None,
                })),
                Err(e) => err(format!("failed to encode result: {e}"), None),
            },
            Ok(Err(fe)) => err(fe.text, fe.argument),
            Err(payload) => err(
                format!(
                    "the {:?} function panicked: {}",
                    req.name,
                    panic_message(payload)
                ),
                None,
            ),
        }
    }

    async fn stop_provider(
        &self,
        _request: Request<tfplugin6::stop_provider::Request>,
    ) -> Result<Response<tfplugin6::stop_provider::Response>, Status> {
        tracing::debug!("StopProvider");
        // Trip the shared token; in-flight handlers scoped under it (via `guard`)
        // can observe cancellation through `current_cancellation`. Acknowledge
        // with an empty error (success).
        self.cancel.cancel();
        Ok(Response::new(Default::default()))
    }

    // Client-streaming request, unary response — does not fit the macro shape.
    async fn write_state_bytes(
        &self,
        _request: Request<tonic::Streaming<tfplugin6::write_state_bytes::RequestChunk>>,
    ) -> Result<Response<tfplugin6::write_state_bytes::Response>, Status> {
        Err(Status::unimplemented(
            "write_state_bytes is not implemented yet",
        ))
    }

    // Server-streaming RPCs: declare the stream type and refuse for now.
    type ListResourceStream = BoxStream<tfplugin6::list_resource::Event>;
    async fn list_resource(
        &self,
        _request: Request<tfplugin6::list_resource::Request>,
    ) -> Result<Response<Self::ListResourceStream>, Status> {
        Err(Status::unimplemented(
            "list_resource is not implemented yet",
        ))
    }

    type ReadStateBytesStream = BoxStream<tfplugin6::read_state_bytes::Response>;
    async fn read_state_bytes(
        &self,
        _request: Request<tfplugin6::read_state_bytes::Request>,
    ) -> Result<Response<Self::ReadStateBytesStream>, Status> {
        Err(Status::unimplemented(
            "read_state_bytes is not implemented yet",
        ))
    }

    type InvokeActionStream = BoxStream<tfplugin6::invoke_action::Event>;
    async fn invoke_action(
        &self,
        _request: Request<tfplugin6::invoke_action::Request>,
    ) -> Result<Response<Self::InvokeActionStream>, Status> {
        Err(Status::unimplemented(
            "invoke_action is not implemented yet",
        ))
    }
}

/// Best-effort extraction of a panic message from its boxed payload. Takes the
/// `Box` by value: passing `&box` would erase the `Box` itself as the `Any`
/// value (downcast always failing) rather than its `&str`/`String` contents.
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// Decode a `DynamicValue` (msgpack, or JSON fallback) under `ty`; absent/empty
/// decodes to [`Value::Null`].
fn decode_dynamic(dv: &Option<tfplugin6::DynamicValue>, ty: &Type) -> Result<Value, CodecError> {
    match dv {
        Some(d) => decode_argument(d, ty),
        None => Ok(Value::Null),
    }
}

/// Decode a present `DynamicValue` (msgpack, else cty JSON, else null) under `ty`.
fn decode_argument(dv: &tfplugin6::DynamicValue, ty: &Type) -> Result<Value, CodecError> {
    if !dv.msgpack.is_empty() {
        decode_msgpack(&dv.msgpack, ty)
    } else if !dv.json.is_empty() {
        let json: facet_value::Value =
            facet_json::from_slice(&dv.json).map_err(|e| CodecError::Decode(e.to_string()))?;
        decode_json(&json, ty)
    } else {
        Ok(Value::Null)
    }
}

/// Encode a [`Value`] into a msgpack `DynamicValue` under `ty`.
fn encode_dynamic(value: &Value, ty: &Type) -> Result<tfplugin6::DynamicValue, CodecError> {
    Ok(tfplugin6::DynamicValue {
        msgpack: encode_msgpack(value, ty)?,
        json: Vec::new(),
    })
}

/// Encode `value` and build a state response via `build`, folding in the
/// handler's context outputs: success-path warnings ride along as diagnostics,
/// and any new private state overrides the incoming `private`. Encode errors
/// surface as diagnostics.
fn respond_state<R>(
    value: &Value,
    ty: &Type,
    private: Vec<u8>,
    outs: CtxOutputs,
    build: impl FnOnce(Option<tfplugin6::DynamicValue>, Vec<u8>, Vec<tfplugin6::Diagnostic>) -> R,
) -> Result<Response<R>, Status> {
    let CtxOutputs {
        warnings,
        private_out,
        ..
    } = outs;
    let private = private_out.unwrap_or(private);
    let mut diagnostics = pb_diagnostics(warnings);
    match encode_dynamic(value, ty) {
        Ok(dv) => Ok(Response::new(build(Some(dv), private, diagnostics))),
        Err(e) => {
            diagnostics.extend(error_diag("failed to encode state", e.to_string()));
            Ok(Response::new(build(None, private, diagnostics)))
        }
    }
}

/// Merge handler error diagnostics with the context's success-path warnings into
/// protocol diagnostics (used on the failure path, where there is no state).
fn diags_with_warnings(diags: Diagnostics, outs: CtxOutputs) -> Vec<tfplugin6::Diagnostic> {
    let mut all = diags;
    all.extend(outs.warnings);
    pb_diagnostics(all)
}

/// Convert SDK diagnostics into protocol diagnostics.
fn pb_diagnostics(diags: Diagnostics) -> Vec<tfplugin6::Diagnostic> {
    use tfplugin6::attribute_path::{step::Selector, Step};
    use tfplugin6::diagnostic::Severity as Pb;
    use tfplugin6::AttributePath;
    diags
        .into_iter()
        .map(|d| tfplugin6::Diagnostic {
            severity: match d.severity {
                Severity::Error => Pb::Error,
                Severity::Warning => Pb::Warning,
            } as i32,
            summary: d.summary,
            detail: d.detail,
            attribute: (!d.attribute.is_empty()).then(|| AttributePath {
                steps: d
                    .attribute
                    .into_iter()
                    .map(|name| Step {
                        selector: Some(Selector::AttributeName(name)),
                    })
                    .collect(),
            }),
        })
        .collect()
}

/// A single error diagnostic.
fn error_diag(summary: impl Into<String>, detail: impl Into<String>) -> Vec<tfplugin6::Diagnostic> {
    pb_diagnostics(vec![Diag::error(summary, detail)])
}

/// Diagnostic for an unregistered resource type.
fn unknown_resource(name: &str) -> Vec<tfplugin6::Diagnostic> {
    error_diag(
        "unknown resource type",
        format!("the provider has no resource named `{name}`"),
    )
}

fn unknown_data_source(name: &str) -> Vec<tfplugin6::Diagnostic> {
    error_diag(
        "unknown data source type",
        format!("the provider has no data source named `{name}`"),
    )
}

fn unknown_ephemeral(name: &str) -> Vec<tfplugin6::Diagnostic> {
    error_diag(
        "unknown ephemeral resource type",
        format!("the provider has no ephemeral resource named `{name}`"),
    )
}

/// Convert a [`SystemTime`](std::time::SystemTime) into the protocol's
/// `google.protobuf.Timestamp`, for an ephemeral resource's `renew_at`. A
/// renewal time is always in the future, so the pre-epoch branch is defensive.
fn to_timestamp(t: std::time::SystemTime) -> prost_types::Timestamp {
    match t.duration_since(std::time::SystemTime::UNIX_EPOCH) {
        Ok(d) => prost_types::Timestamp {
            seconds: d.as_secs() as i64,
            nanos: d.subsec_nanos() as i32,
        },
        Err(e) => {
            let d = e.duration();
            prost_types::Timestamp {
                seconds: -(d.as_secs() as i64),
                nanos: -(d.subsec_nanos() as i32),
            }
        }
    }
}
