//! The gRPC [`tfplugin6::provider_server::Provider`] implementation.
//!
//! Answers schema discovery (`GetMetadata`, `GetProviderSchema`), the resource
//! lifecycle (`ConfigureProvider`, validation, `UpgradeResourceState`,
//! `PlanResourceChange`, `ReadResource`, `ApplyResourceChange`), and data source
//! reads (`ReadDataSource`) by dispatching to the registered handlers through the
//! value codec. RPCs for features the SDK does not yet support (functions,
//! ephemeral resources, identity, state stores, actions, import/move) still
//! return `Unimplemented`.

use std::pin::Pin;

use terraform_codec::{decode_json, decode_msgpack, encode_msgpack, CodecError};
use terraform_tfplugin6::{emit_metadata, emit_provider_schema, tfplugin6};
use terraform_value::{Type, Value};
use tonic::codegen::tokio_stream::Stream;
use tonic::{Request, Response, Status};

use crate::builder::Provider;
use crate::plan::plan;
use crate::resource::{Diag, Diagnostics, Severity};

/// A boxed server-streaming response of `T`. Used only to satisfy the trait's
/// associated stream types for the not-yet-implemented streaming RPCs.
type BoxStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

/// Adapts a [`Provider`] to the generated gRPC service trait.
#[derive(Clone)]
pub struct ProviderService {
    provider: Provider,
}

impl ProviderService {
    /// Wrap a built provider.
    pub fn new(provider: Provider) -> Self {
        Self { provider }
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
        _request: Request<tfplugin6::validate_provider_config::Request>,
    ) -> Result<Response<tfplugin6::validate_provider_config::Response>, Status> {
        Ok(Response::new(Default::default()))
    }

    async fn validate_resource_config(
        &self,
        _request: Request<tfplugin6::validate_resource_config::Request>,
    ) -> Result<Response<tfplugin6::validate_resource_config::Response>, Status> {
        Ok(Response::new(Default::default()))
    }

    async fn validate_data_resource_config(
        &self,
        _request: Request<tfplugin6::validate_data_resource_config::Request>,
    ) -> Result<Response<tfplugin6::validate_data_resource_config::Response>, Status> {
        Ok(Response::new(Default::default()))
    }

    // --- Resource lifecycle ------------------------------------------------

    async fn upgrade_resource_state(
        &self,
        request: Request<tfplugin6::upgrade_resource_state::Request>,
    ) -> Result<Response<tfplugin6::upgrade_resource_state::Response>, Status> {
        use tfplugin6::upgrade_resource_state::Response as Resp;
        let req = request.into_inner();

        let Some(ty) = self.provider.resource_cty(&req.type_name) else {
            return Ok(Response::new(Resp {
                diagnostics: unknown_resource(&req.type_name),
                ..Default::default()
            }));
        };

        // Stored state arrives as cty JSON in `raw_state.json`.
        let value = match req.raw_state.as_ref() {
            Some(raw) if !raw.json.is_empty() => match facet_json::from_slice(&raw.json)
                .map_err(|e| CodecError::Decode(e.to_string()))
                .and_then(|j: facet_value::Value| decode_json(&j, &ty))
            {
                Ok(v) => v,
                Err(e) => {
                    return Ok(Response::new(Resp {
                        diagnostics: error_diag("failed to read prior state", e.to_string()),
                        ..Default::default()
                    }))
                }
            },
            _ => Value::Null,
        };

        match encode_dynamic(&value, &ty) {
            Ok(dv) => Ok(Response::new(Resp {
                upgraded_state: Some(dv),
                ..Default::default()
            })),
            Err(e) => Ok(Response::new(Resp {
                diagnostics: error_diag("failed to encode upgraded state", e.to_string()),
                ..Default::default()
            })),
        }
    }

    async fn plan_resource_change(
        &self,
        request: Request<tfplugin6::plan_resource_change::Request>,
    ) -> Result<Response<tfplugin6::plan_resource_change::Response>, Status> {
        use tfplugin6::plan_resource_change::Response as Resp;
        let req = request.into_inner();

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

        let plan = plan(&prior, proposed, block);
        match encode_dynamic(&plan.planned, &ty) {
            Ok(dv) => Ok(Response::new(Resp {
                planned_state: Some(dv),
                requires_replace: plan.requires_replace,
                planned_private: req.planned_private,
                ..Default::default()
            })),
            Err(e) => Ok(Response::new(Resp {
                diagnostics: error_diag("failed to encode planned state", e.to_string()),
                ..Default::default()
            })),
        }
    }

    async fn read_resource(
        &self,
        request: Request<tfplugin6::read_resource::Request>,
    ) -> Result<Response<tfplugin6::read_resource::Response>, Status> {
        use tfplugin6::read_resource::Response as Resp;
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

        let current = match decode_dynamic(&req.current_state, &ty) {
            Ok(v) => v,
            Err(e) => {
                return Ok(Response::new(Resp {
                    diagnostics: error_diag("failed to decode current state", e.to_string()),
                    ..Default::default()
                }))
            }
        };

        let outcome = handler.read(current).await;
        let new_value = match outcome {
            Ok(Some(v)) => v,
            Ok(None) => Value::Null, // resource is gone
            Err(diags) => {
                return Ok(Response::new(Resp {
                    diagnostics: pb_diagnostics(diags),
                    ..Default::default()
                }))
            }
        };

        respond_state(
            &new_value,
            &ty,
            req.private,
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
        let outcome: Result<Value, Diagnostics> = if planned.is_null() {
            handler.delete(prior.clone()).await.map(|()| Value::Null)
        } else if prior.is_null() {
            handler.create(planned).await
        } else {
            handler.update(planned, prior.clone()).await
        };

        match outcome {
            Ok(new_value) => respond_state(
                &new_value,
                &ty,
                req.planned_private,
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
                diagnostics: pb_diagnostics(diags),
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

        match handler.read(config).await {
            Ok(state) => match encode_dynamic(&state, &ty) {
                Ok(dv) => Ok(Response::new(Resp {
                    state: Some(dv),
                    ..Default::default()
                })),
                Err(e) => Ok(Response::new(Resp {
                    diagnostics: error_diag("failed to encode data source state", e.to_string()),
                    ..Default::default()
                })),
            },
            Err(diags) => Ok(Response::new(Resp {
                diagnostics: pb_diagnostics(diags),
                ..Default::default()
            })),
        }
    }

    unimplemented_unary! {
        get_resource_identity_schemas => get_resource_identity_schemas,
        upgrade_resource_identity => upgrade_resource_identity,
        import_resource_state => import_resource_state,
        move_resource_state => move_resource_state,
        generate_resource_config => generate_resource_config,
        validate_ephemeral_resource_config => validate_ephemeral_resource_config,
        open_ephemeral_resource => open_ephemeral_resource,
        renew_ephemeral_resource => renew_ephemeral_resource,
        close_ephemeral_resource => close_ephemeral_resource,
        validate_list_resource_config => validate_list_resource_config,
        get_functions => get_functions,
        call_function => call_function,
        validate_state_store_config => validate_state_store,
        configure_state_store => configure_state_store,
        lock_state => lock_state,
        unlock_state => unlock_state,
        get_states => get_states,
        delete_state => delete_state,
        plan_action => plan_action,
        validate_action_config => validate_action_config,
        stop_provider => stop_provider,
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

/// Decode a `DynamicValue` (msgpack, or JSON fallback) under `ty`; absent/empty
/// decodes to [`Value::Null`].
fn decode_dynamic(dv: &Option<tfplugin6::DynamicValue>, ty: &Type) -> Result<Value, CodecError> {
    match dv {
        Some(d) if !d.msgpack.is_empty() => decode_msgpack(&d.msgpack, ty),
        Some(d) if !d.json.is_empty() => {
            let json: facet_value::Value =
                facet_json::from_slice(&d.json).map_err(|e| CodecError::Decode(e.to_string()))?;
            decode_json(&json, ty)
        }
        _ => Ok(Value::Null),
    }
}

/// Encode a [`Value`] into a msgpack `DynamicValue` under `ty`.
fn encode_dynamic(value: &Value, ty: &Type) -> Result<tfplugin6::DynamicValue, CodecError> {
    Ok(tfplugin6::DynamicValue {
        msgpack: encode_msgpack(value, ty)?,
        json: Vec::new(),
    })
}

/// Encode `value` and build a state response via `build`, surfacing encode
/// errors as diagnostics.
fn respond_state<R>(
    value: &Value,
    ty: &Type,
    private: Vec<u8>,
    build: impl FnOnce(Option<tfplugin6::DynamicValue>, Vec<u8>, Vec<tfplugin6::Diagnostic>) -> R,
) -> Result<Response<R>, Status> {
    match encode_dynamic(value, ty) {
        Ok(dv) => Ok(Response::new(build(Some(dv), private, Vec::new()))),
        Err(e) => Ok(Response::new(build(
            None,
            private,
            error_diag("failed to encode state", e.to_string()),
        ))),
    }
}

/// Convert SDK diagnostics into protocol diagnostics.
fn pb_diagnostics(diags: Diagnostics) -> Vec<tfplugin6::Diagnostic> {
    use tfplugin6::diagnostic::Severity as Pb;
    diags
        .into_iter()
        .map(|d| tfplugin6::Diagnostic {
            severity: match d.severity {
                Severity::Error => Pb::Error,
                Severity::Warning => Pb::Warning,
            } as i32,
            summary: d.summary,
            detail: d.detail,
            ..Default::default()
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
