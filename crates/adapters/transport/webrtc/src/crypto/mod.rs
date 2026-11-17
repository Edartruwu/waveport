//! The cryptography of a WebRTC call.
//!
//! `str0m` does none itself. It is given a provider: the handshake
//! (DTLS), the ciphers that audio is sealed with (SRTP), and the hashes
//! the checks of ICE and the fingerprint of a certificate are made
//! with. The providers that come with it bring aws-lc or OpenSSL, and
//! with them C and a C build. The one in pure Rust does too, for one
//! thing: it has its DTLS library make the certificate, and that asks
//! for `rcgen` on aws-lc.
//!
//! So this is that provider, `str0m-rust-crypto` 0.7.0 (MIT OR
//! Apache-2.0, © Martin Algesten), written again by this repository's
//! rules, with the certificate made here ([`certificate`]). What it does
//! is what the original does. What differs is what happens when `str0m`
//! gives it something it cannot have given: the original stops the
//! program, and this one answers with an error where it can say one,
//! and leaves the output as it was where it cannot.

mod cert;
mod dtls;
mod hash;
mod srtp;

use str0m_proto::crypto::CryptoProvider;

pub(crate) use self::cert::{Certificate, certificate};

/// The provider `str0m` is given: all of it is Rust.
pub(crate) fn provider() -> CryptoProvider {
    CryptoProvider {
        srtp_provider: &srtp::Srtp,
        sha1_hmac_provider: &hash::Sha1Hmac,
        sha1_provider: &hash::Sha1,
        sha256_provider: &hash::Sha256,
        dtls_provider: &dtls::Dtls,
    }
}
