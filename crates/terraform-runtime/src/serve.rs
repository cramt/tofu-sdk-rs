//! Bootstrap and serve a provider over the Terraform plugin protocol.
//!
//! [`serve`] performs the full Layer-1 sequence — magic-cookie check, protocol
//! negotiation, auto-mTLS setup, listener creation, handshake line — and then
//! runs the gRPC server (Layer 2) until the host asks it to stop.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rustls::ServerConfig;
use terraform_tfplugin6::tfplugin6::provider_server::ProviderServer;
use tokio::net::UnixListener;
use tokio_rustls::TlsAcceptor;
use tokio_stream::wrappers::UnixListenerStream;
use tokio_stream::StreamExt as _;
use tonic::transport::Server;

use crate::builder::Provider;
use crate::handshake::{self, HandshakeError};
use crate::service::ProviderService;
use crate::tls::{self, TlsError};

/// Errors that can stop a provider from serving.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// The go-plugin handshake failed (bad cookie or protocol mismatch).
    #[error(transparent)]
    Handshake(#[from] HandshakeError),

    /// Auto-mTLS setup failed.
    #[error(transparent)]
    Tls(#[from] TlsError),

    /// The gRPC transport failed.
    #[error("transport error: {0}")]
    Transport(#[from] tonic::transport::Error),

    /// A filesystem/socket IO error occurred.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Serve `provider` over the plugin protocol, reading configuration from the
/// process environment (the normal entry point from `main`).
///
/// Blocks until the process is signalled (SIGTERM/Ctrl-C) — Terraform terminates
/// the plugin subprocess when it is done with it.
pub async fn serve(provider: Provider) -> Result<(), ServeError> {
    // Bridge `tracing` to Terraform's log stream first, so the handshake and
    // setup steps below are visible under `TF_LOG`.
    crate::log::init();
    tracing::info!(
        resources = provider.schema().resources.len(),
        data_sources = provider.schema().data_sources.len(),
        "starting provider"
    );

    // Layer 1: handshake preconditions.
    handshake::check_magic_cookie(std::env::var(handshake::MAGIC_COOKIE_KEY).ok().as_deref())?;
    let protocol_version = handshake::negotiate_protocol(
        std::env::var(handshake::ENV_PROTOCOL_VERSIONS)
            .ok()
            .as_deref(),
    )?;
    let tls_setup = tls::configure_tls(tls::client_cert_present())?;

    // Listener: a unix socket in the system temp dir.
    let socket_path = unique_socket_path();
    let _ = std::fs::remove_file(&socket_path); // clear any stale socket
    let listener = UnixListener::bind(&socket_path)?;

    // Announce how to connect, then serve. Binding happened above, so the host
    // can dial as soon as it reads this line.
    let line = handshake::handshake_line(
        protocol_version,
        "unix",
        &socket_path.to_string_lossy(),
        tls_setup.server_cert_b64.as_deref(),
    );
    print_handshake(&line)?;

    let result = run_server(provider, listener, tls_setup.config).await;

    // Best-effort cleanup of the socket file.
    let _ = std::fs::remove_file(&socket_path);
    result
}

/// Run the gRPC server on an already-bound listener until shutdown.
///
/// When `tls` is set we terminate TLS ourselves with a tokio-rustls acceptor
/// (see [`crate::tls`] for why) and feed the decrypted streams to tonic.
async fn run_server(
    provider: Provider,
    listener: UnixListener,
    tls: Option<Arc<ServerConfig>>,
) -> Result<(), ServeError> {
    let router = Server::builder().add_service(ProviderServer::new(ProviderService::new(provider)));

    match tls {
        Some(config) => {
            let acceptor = TlsAcceptor::from(config);
            let incoming = UnixListenerStream::new(listener).then(move |conn| {
                let acceptor = acceptor.clone();
                async move {
                    match conn {
                        Ok(stream) => acceptor.accept(stream).await,
                        Err(e) => Err(e),
                    }
                }
            });
            router
                .serve_with_incoming_shutdown(incoming, shutdown_signal())
                .await?;
        }
        None => {
            router
                .serve_with_incoming_shutdown(UnixListenerStream::new(listener), shutdown_signal())
                .await?;
        }
    }
    Ok(())
}

/// Print the handshake line to stdout and flush immediately.
fn print_handshake(line: &str) -> std::io::Result<()> {
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{line}")?;
    stdout.flush()
}

/// A unique unix socket path under the system temp directory.
fn unique_socket_path() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let name = format!("tofu-plugin-{}-{}.sock", std::process::id(), nanos);
    std::env::temp_dir().join(name)
}

/// Completes when the host asks the plugin to stop.
///
/// Terraform manages the plugin's lifecycle: when it is done it kills the
/// subprocess (after an attempted graceful stop), so reacting to SIGTERM/Ctrl-C
/// is sufficient. We deliberately do **not** watch stdin for EOF — in
/// non-interactive contexts (CI, `tofu providers schema`) the plugin inherits an
/// already-closed stdin, which would make it shut down before the host can even
/// connect.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
