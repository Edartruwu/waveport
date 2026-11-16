//! A crypto provider for str0m 0.25.0 with no aws-lc, no OpenSSL and no `ring`.
//!
//! `sha1.rs`, `sha256.rs`, `srtp.rs` and `dtls.rs` are `str0m-rust-crypto`
//! 0.7.0 (MIT OR Apache-2.0, © Martin Algesten) unchanged except for the one
//! function that makes the certificate: upstream calls
//! `dimpl::certificate::generate_self_signed_certificate`, which exists only
//! with dimpl's `rcgen` feature, and that feature turns on `aws-lc-rs`.
//! `cert.rs` makes the same certificate (ECDSA P-256, SHA-256, one year,
//! random serial) with `p256` and `x509-cert`.

#![allow(clippy::collapsible_if)]

pub mod cert;
mod dtls;
mod sha1;
mod sha256;
mod srtp;

use dtls::RustCryptoDtlsProvider;
use sha1::{RustCryptoSha1HmacProvider, RustCryptoSha1Provider};
use sha256::RustCryptoSha256Provider;
use srtp::RustCryptoSrtpProvider;
use str0m_proto::crypto::CryptoProvider;

/// The provider: DTLS through dimpl's own RustCrypto backend, SRTP, SHA-1,
/// SHA-256 and HMAC-SHA1 through the RustCrypto crates.
pub fn provider() -> CryptoProvider {
    static SRTP: RustCryptoSrtpProvider = RustCryptoSrtpProvider;
    static SHA1_HMAC: RustCryptoSha1HmacProvider = RustCryptoSha1HmacProvider;
    static SHA1: RustCryptoSha1Provider = RustCryptoSha1Provider;
    static SHA256: RustCryptoSha256Provider = RustCryptoSha256Provider;
    static DTLS: RustCryptoDtlsProvider = RustCryptoDtlsProvider;

    CryptoProvider {
        srtp_provider: &SRTP,
        sha1_hmac_provider: &SHA1_HMAC,
        sha1_provider: &SHA1,
        sha256_provider: &SHA256,
        dtls_provider: &DTLS,
    }
}
