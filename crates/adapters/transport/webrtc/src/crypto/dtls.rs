//! The handshake: DTLS as `dimpl` does it, with its ciphers in Rust.

use std::sync::Arc;
use std::time::Instant;

use str0m_proto::crypto::dtls::{
    DtlsCert, DtlsImplError, DtlsInstance, DtlsOutput, DtlsProvider, ProtocolVersion,
};
use str0m_proto::crypto::{CryptoError, DtlsVersion};

#[derive(Debug)]
pub(super) struct Dtls;

impl DtlsProvider for Dtls {
    fn generate_certificate(&self) -> Option<DtlsCert> {
        let made = super::certificate()?;
        Some(made.into_dtls())
    }

    fn new_dtls(
        &self,
        cert: &DtlsCert,
        now: Instant,
        dtls_version: DtlsVersion,
        mtu: Option<usize>,
    ) -> Result<Box<dyn DtlsInstance>, CryptoError> {
        let shown = dimpl::DtlsCertificate {
            certificate: cert.certificate.clone(),
            private_key: cert.private_key.clone(),
        };
        // A cookie proves that whoever asks can be answered where they
        // say they are, which the checks of ICE have shown before a
        // handshake begins.
        let mut config = dimpl::Config::builder().use_server_cookie(false);
        if let Some(mtu) = mtu {
            config = config.mtu(mtu);
        }
        let config = config.build().map_err(|error| {
            CryptoError::Other(format!("a handshake cannot be set up: {error}"))
        })?;
        let config = Arc::new(config);
        let dtls = match dtls_version {
            DtlsVersion::Dtls12 => dimpl::Dtls::new_12(config, shown, now),
            DtlsVersion::Dtls13 => dimpl::Dtls::new_13(config, shown, now),
            DtlsVersion::Auto => dimpl::Dtls::new_auto(config, shown, now),
            other => {
                return Err(CryptoError::Other(format!(
                    "DTLS {other} is not one this provider speaks"
                )));
            }
        };
        Ok(Box::new(Handshake { dtls }))
    }
}

/// One handshake, and the connection that comes of it.
struct Handshake {
    dtls: dimpl::Dtls,
}

impl core::fmt::Debug for Handshake {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // What it holds are keys.
        f.debug_struct("Handshake").finish_non_exhaustive()
    }
}

impl DtlsInstance for Handshake {
    fn set_active(&mut self, active: bool) {
        self.dtls.set_active(active);
    }

    fn handle_packet(&mut self, packet: &[u8]) -> Result<(), DtlsImplError> {
        self.dtls.handle_packet(packet)
    }

    fn poll_output<'a>(&mut self, buf: &'a mut [u8]) -> DtlsOutput<'a> {
        self.dtls.poll_output(buf)
    }

    fn handle_timeout(&mut self, now: Instant) -> Result<(), DtlsImplError> {
        self.dtls.handle_timeout(now)
    }

    fn send_application_data(&mut self, data: &[u8]) -> Result<usize, DtlsImplError> {
        self.dtls.send_application_data(data)
    }

    fn is_active(&self) -> bool {
        self.dtls.is_active()
    }

    fn protocol_version(&self) -> Option<ProtocolVersion> {
        self.dtls.protocol_version()
    }

    fn is_closing(&self) -> bool {
        self.dtls.is_closing()
    }

    fn is_closed(&self) -> bool {
        self.dtls.is_closed()
    }

    fn close(&mut self) -> Result<(), DtlsImplError> {
        self.dtls.close()
    }
}
