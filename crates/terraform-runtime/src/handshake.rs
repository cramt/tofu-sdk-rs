//! The go-plugin handshake (Layer 1 of the Terraform plugin protocol).
//!
//! When Terraform/OpenTofu launches a provider it:
//!
//! 1. sets a "magic cookie" env var the plugin must echo-verify (a guard against
//!    the binary being run directly),
//! 2. advertises the plugin-protocol versions it supports,
//! 3. waits for the plugin to print a single handshake line on stdout describing
//!    how to connect, then dials it.
//!
//! This module holds the protocol constants and the pure functions that
//! implement those three steps; [`crate::serve`] performs the IO around them.

/// The go-plugin meta-protocol (RPC) version this SDK speaks.
pub const CORE_PROTOCOL_VERSION: u32 = 1;

/// The Terraform plugin-protocol (application) version this SDK implements.
pub const APP_PROTOCOL_VERSION: u32 = 6;

/// Env var key Terraform sets to the magic cookie value.
pub const MAGIC_COOKIE_KEY: &str = "TF_PLUGIN_MAGIC_COOKIE";

/// The well-known magic cookie value Terraform uses for plugins.
pub const MAGIC_COOKIE_VALUE: &str =
    "d602bf8f470bc67ca7faa0386276bbdd4330efaf76d1a219cb4d6991ca9872b2";

/// Env var listing the protocol versions the host supports (e.g. `"5,6"`).
pub const ENV_PROTOCOL_VERSIONS: &str = "PLUGIN_PROTOCOL_VERSIONS";

/// Env var carrying the host's client certificate (PEM) for auto-mTLS.
pub const ENV_CLIENT_CERT: &str = "PLUGIN_CLIENT_CERT";

/// The wire protocol advertised in the handshake line.
pub const PROTOCOL: &str = "grpc";

/// A handshake failure that means we must not start serving.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HandshakeError {
    /// The magic cookie env var was missing or did not match.
    ///
    /// Almost always means the binary was run directly instead of by Terraform.
    #[error(
        "{MAGIC_COOKIE_KEY} was missing or incorrect. This binary is a Terraform \
         plugin and is meant to be launched by Terraform/OpenTofu, not run directly."
    )]
    MagicCookieMismatch,

    /// The host does not support protocol v6.
    #[error(
        "host does not support plugin protocol v{APP_PROTOCOL_VERSION} (offered: {offered:?})"
    )]
    UnsupportedProtocol {
        /// The raw value of `PLUGIN_PROTOCOL_VERSIONS`.
        offered: String,
    },
}

/// Verify the magic cookie value Terraform passed.
pub fn check_magic_cookie(value: Option<&str>) -> Result<(), HandshakeError> {
    match value {
        Some(v) if v == MAGIC_COOKIE_VALUE => Ok(()),
        _ => Err(HandshakeError::MagicCookieMismatch),
    }
}

/// Negotiate the protocol version against the host's offered list.
///
/// We only implement v6, so we accept iff the host offers it. An absent/empty
/// list is treated as "host is fine with our version" (Terraform always sets it,
/// but being lenient keeps local testing simple).
pub fn negotiate_protocol(offered: Option<&str>) -> Result<u32, HandshakeError> {
    let raw = offered.unwrap_or("").trim();
    if raw.is_empty() {
        return Ok(APP_PROTOCOL_VERSION);
    }
    let supported = raw
        .split(',')
        .filter_map(|s| s.trim().parse::<u32>().ok())
        .any(|v| v == APP_PROTOCOL_VERSION);
    if supported {
        Ok(APP_PROTOCOL_VERSION)
    } else {
        Err(HandshakeError::UnsupportedProtocol {
            offered: raw.to_string(),
        })
    }
}

/// Build the single stdout handshake line Terraform parses to learn how to
/// connect:
///
/// ```text
/// CORE | APP | network | address | protocol [ | base64(server-cert-DER) ]
/// ```
///
/// The trailing certificate field is present only under auto-mTLS.
pub fn handshake_line(
    protocol_version: u32,
    network: &str,
    address: &str,
    server_cert_b64: Option<&str>,
) -> String {
    let head = format!("{CORE_PROTOCOL_VERSION}|{protocol_version}|{network}|{address}|{PROTOCOL}");
    match server_cert_b64 {
        Some(cert) => format!("{head}|{cert}"),
        None => head,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_cookie_must_match() {
        assert!(check_magic_cookie(Some(MAGIC_COOKIE_VALUE)).is_ok());
        assert_eq!(
            check_magic_cookie(Some("nope")),
            Err(HandshakeError::MagicCookieMismatch)
        );
        assert_eq!(
            check_magic_cookie(None),
            Err(HandshakeError::MagicCookieMismatch)
        );
    }

    #[test]
    fn negotiates_v6() {
        assert_eq!(negotiate_protocol(Some("5,6")), Ok(6));
        assert_eq!(negotiate_protocol(Some(" 6 ")), Ok(6));
        assert_eq!(negotiate_protocol(None), Ok(6));
        assert_eq!(negotiate_protocol(Some("")), Ok(6));
        assert!(matches!(
            negotiate_protocol(Some("4,5")),
            Err(HandshakeError::UnsupportedProtocol { .. })
        ));
    }

    #[test]
    fn handshake_line_with_and_without_cert() {
        assert_eq!(
            handshake_line(6, "unix", "/tmp/plugin.sock", None),
            "1|6|unix|/tmp/plugin.sock|grpc"
        );
        assert_eq!(
            handshake_line(6, "unix", "/tmp/p.sock", Some("Q0VSVA")),
            "1|6|unix|/tmp/p.sock|grpc|Q0VSVA"
        );
    }
}
