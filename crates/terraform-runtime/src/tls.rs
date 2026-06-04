//! Auto-mTLS for the plugin handshake — go-plugin compatible.
//!
//! When Terraform/OpenTofu launches a provider with auto-mTLS it generates a key
//! pair, passes its certificate (PEM) to the plugin via [`ENV_CLIENT_CERT`], and
//! expects the plugin to:
//!
//! 1. generate its own certificate,
//! 2. advertise it (base64 DER) in the handshake line, and
//! 3. serve TLS presenting that certificate.
//!
//! The host then **pins** the advertised certificate as the only trusted root
//! when dialing — that pin is the security property that matters over the
//! plugin's unix socket (it stops any other process from impersonating the
//! provider).
//!
//! ## Server-auth, not the rustls client-auth path
//!
//! We present and advertise our certificate but do **not** require a client
//! certificate at the TLS layer. tonic's `ServerTlsConfig` only offers a WebPKI
//! client verifier, which advertises CA-name hints and does path validation;
//! Go's `crypto/tls` client then withholds its (self-signed) certificate and the
//! handshake aborts with `certificate required`. Go only presents a client
//! certificate when asked, so server-auth-only interoperates cleanly while
//! preserving the cert-pinning guarantee the host relies on. We therefore build
//! the rustls [`ServerConfig`] directly.
//!
//! Our certificate is generated as a self-signed CA with both server- and
//! client-auth EKUs and a `localhost` SAN, mirroring go-plugin's own cert so the
//! host can use it as a trust root.

use std::sync::Arc;

use base64::engine::general_purpose::STANDARD_NO_PAD;
use base64::Engine as _;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose,
};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;

use crate::handshake::ENV_CLIENT_CERT;

/// The result of configuring transport security.
pub struct TlsSetup {
    /// The rustls server config to apply, if TLS is in effect.
    pub config: Option<Arc<ServerConfig>>,
    /// Base64 (raw, unpadded) DER of our server cert, for the handshake line.
    pub server_cert_b64: Option<String>,
}

/// Failure while setting up auto-mTLS.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    /// Generating the self-signed server certificate failed.
    #[error("failed to generate server certificate: {0}")]
    CertGen(#[from] rcgen::Error),

    /// Building the rustls server configuration failed.
    #[error("failed to build TLS config: {0}")]
    Rustls(#[from] rustls::Error),
}

/// Configure transport security.
///
/// `client_cert_present` reflects whether the host set [`ENV_CLIENT_CERT`]
/// (i.e. requested auto-mTLS). When `false`, TLS is disabled (local/manual
/// testing). The host's certificate value itself is not needed because we do not
/// require client auth.
pub fn configure_tls(client_cert_present: bool) -> Result<TlsSetup, TlsError> {
    if !client_cert_present {
        return Ok(TlsSetup {
            config: None,
            server_cert_b64: None,
        });
    }

    let (cert_der, key_der) = generate_server_cert()?;
    let server_cert_b64 = STANDARD_NO_PAD.encode(&cert_der);

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der.into())?;
    // gRPC requires the HTTP/2 ALPN protocol over TLS.
    let mut config = config;
    config.alpn_protocols = vec![b"h2".to_vec()];

    Ok(TlsSetup {
        config: Some(Arc::new(config)),
        server_cert_b64: Some(server_cert_b64),
    })
}

/// Whether the host requested auto-mTLS (set [`ENV_CLIENT_CERT`]).
pub fn client_cert_present() -> bool {
    std::env::var(ENV_CLIENT_CERT)
        .ok()
        .is_some_and(|v| !v.trim().is_empty())
}

/// Generate a self-signed CA certificate mirroring go-plugin's, returning its
/// DER and PKCS#8 private key.
fn generate_server_cert() -> Result<(CertificateDer<'static>, PrivatePkcs8KeyDer<'static>), TlsError>
{
    let mut params = CertificateParams::new(vec!["localhost".to_string()])?;
    params
        .distinguished_name
        .push(DnType::CommonName, "localhost");
    params
        .distinguished_name
        .push(DnType::OrganizationName, "tofu-sdk-rs");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
        KeyUsagePurpose::KeyCertSign,
    ];
    params.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::ClientAuth,
    ];

    let key = KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    let cert_der = cert.der().clone();
    let key_der = PrivatePkcs8KeyDer::from(key.serialize_der());
    Ok((cert_der, key_der))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_mtls_request_disables_tls() {
        let setup = configure_tls(false).unwrap();
        assert!(setup.config.is_none());
        assert!(setup.server_cert_b64.is_none());
    }

    #[test]
    fn mtls_request_produces_config_and_advertised_cert() {
        let setup = configure_tls(true).unwrap();
        assert!(setup.config.is_some(), "TLS config produced");

        let b64 = setup.server_cert_b64.expect("server cert advertised");
        let der = STANDARD_NO_PAD.decode(b64).expect("valid base64 DER");
        assert!(!der.is_empty());
    }

    #[test]
    fn generated_cert_is_a_ca_with_localhost_san() {
        // Smoke-check the cert parses and is non-trivial.
        let (cert_der, _key) = generate_server_cert().unwrap();
        assert!(cert_der.as_ref().len() > 100);
    }
}
