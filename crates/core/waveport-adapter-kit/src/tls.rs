//! TLS, set up one way for every connection.
//!
//! Certificates are checked the way the operating system checks them,
//! against the roots it trusts, so a machine with its own certificate
//! authority works without being told about it here. The cryptography is
//! `ring`: nothing has to be installed to build it on any of the four
//! platforms.

use std::sync::{Arc, OnceLock};

use rustls::ClientConfig;
use rustls_platform_verifier::BuilderVerifierExt;
use waveport_ports::error::PortError;

/// The TLS settings for a client that speaks the protocols in `alpn`,
/// most wanted first.
///
/// The roots the system trusts are read once, the first time this is
/// called.
///
/// # Errors
///
/// TLS cannot be set up on this machine. Trying again will not help.
pub fn client_config(alpn: &[&str]) -> Result<ClientConfig, PortError> {
    static BASE: OnceLock<Result<ClientConfig, Box<str>>> = OnceLock::new();
    let base = BASE.get_or_init(|| build().map_err(|error| error.to_string().into()));
    match base {
        Ok(base) => {
            let mut config = base.clone();
            config.alpn_protocols = alpn.iter().map(|name| name.as_bytes().to_vec()).collect();
            Ok(config)
        }
        Err(message) => Err(PortError::permanent(format!(
            "TLS cannot be set up: {message}"
        ))),
    }
}

fn build() -> Result<ClientConfig, rustls::Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Ok(ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_platform_verifier()?
        .with_no_client_auth())
}

#[cfg(test)]
mod tests {
    use super::client_config;

    #[test]
    fn tls_can_be_set_up_here() {
        let config = client_config(&["h2", "http/1.1"]).unwrap();
        assert_eq!(
            config.alpn_protocols,
            [b"h2".to_vec(), b"http/1.1".to_vec()]
        );
        // The second time nothing is read again, and the protocols are
        // this caller's own.
        let config = client_config(&["http/1.1"]).unwrap();
        assert_eq!(config.alpn_protocols, [b"http/1.1".to_vec()]);
    }
}
