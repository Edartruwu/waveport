//! The hashes: SHA-1 and its HMAC for the checks of ICE, SHA-256 for the
//! fingerprint of a certificate.

use hmac::{Hmac, KeyInit as _, Mac as _};
use sha1::Digest as _;
use str0m_proto::crypto::{Sha1HmacProvider, Sha1Provider, Sha256Provider};

use super::srtp::unknowable;

#[derive(Debug)]
pub(super) struct Sha1;

impl Sha1Provider for Sha1 {
    fn sha1(&self, data: &[u8]) -> [u8; 20] {
        sha1::Sha1::digest(data).into()
    }
}

#[derive(Debug)]
pub(super) struct Sha1Hmac;

impl Sha1HmacProvider for Sha1Hmac {
    fn sha1_hmac(&self, key: &[u8], payloads: &[&[u8]]) -> [u8; 20] {
        // An HMAC takes a key of any length, so there is none it
        // refuses. Were there one, what is answered is chance, which
        // fits no message: noughts would fit one that was made to.
        let Ok(mut mac) = Hmac::<sha1::Sha1>::new_from_slice(key) else {
            let mut none = [0; 20];
            unknowable(&mut none);
            return none;
        };
        for payload in payloads {
            mac.update(payload);
        }
        mac.finalize().into_bytes().into()
    }
}

#[derive(Debug)]
pub(super) struct Sha256;

impl Sha256Provider for Sha256 {
    fn sha256(&self, data: &[u8]) -> [u8; 32] {
        sha2::Sha256::digest(data).into()
    }
}

#[cfg(test)]
mod tests {
    use str0m_proto::crypto::{Sha1HmacProvider as _, Sha1Provider as _, Sha256Provider as _};

    use super::{Sha1, Sha1Hmac, Sha256};

    fn hex(bytes: &[u8]) -> String {
        use core::fmt::Write as _;
        bytes.iter().fold(String::new(), |mut out, byte| {
            write!(out, "{byte:02x}").unwrap();
            out
        })
    }

    #[test]
    fn the_hashes_are_the_ones_their_standards_work_through() {
        // FIPS 180: "abc".
        assert_eq!(
            hex(&Sha1.sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hex(&Sha256.sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // RFC 2202, test case 2, given in two pieces as a message of
        // ICE is.
        let mac = Sha1Hmac.sha1_hmac(b"Jefe", &[b"what do ya want ", b"for nothing?"]);
        assert_eq!(hex(&mac), "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79");
        // A key longer than a block of the hash is a key: test case 6.
        let mac = Sha1Hmac.sha1_hmac(
            &[0xaa; 80],
            &[b"Test Using Larger Than Block-Size Key - Hash Key First"],
        );
        assert_eq!(hex(&mac), "aa4ae5e15272d00e95705637ce8a3b55ed402112");
    }
}
