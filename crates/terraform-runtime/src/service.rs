//! The gRPC [`tfplugin6::provider_server::Provider`] implementation.
//!
//! Phase 2 answers the schema-discovery RPCs (`GetMetadata`,
//! `GetProviderSchema`) from the reflected provider IR. Every other RPC returns
//! `Unimplemented` for now — they are filled in by the codec, configure, CRUD,
//! and planning phases that follow.

use std::pin::Pin;

use terraform_tfplugin6::{emit_metadata, emit_provider_schema, tfplugin6};
use tonic::codegen::tokio_stream::Stream;
use tonic::{Request, Response, Status};

use crate::builder::Provider;

/// A boxed server-streaming response of `T`. Used only to satisfy the trait's
/// associated stream types for the not-yet-implemented streaming RPCs.
type BoxStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

/// Adapts a [`Provider`] to the generated gRPC service trait.
#[derive(Debug, Clone)]
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

    unimplemented_unary! {
        validate_provider_config => validate_provider_config,
        validate_resource_config => validate_resource_config,
        validate_data_resource_config => validate_data_resource_config,
        upgrade_resource_state => upgrade_resource_state,
        get_resource_identity_schemas => get_resource_identity_schemas,
        upgrade_resource_identity => upgrade_resource_identity,
        configure_provider => configure_provider,
        read_resource => read_resource,
        plan_resource_change => plan_resource_change,
        apply_resource_change => apply_resource_change,
        import_resource_state => import_resource_state,
        move_resource_state => move_resource_state,
        read_data_source => read_data_source,
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
