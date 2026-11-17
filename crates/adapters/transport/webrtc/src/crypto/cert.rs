//! The certificate a handshake shows.

use core::str::FromStr as _;
use core::time::Duration;

use p256::ecdsa::{DerSignature, SigningKey};
use p256::elliptic_curve::Generate as _;
use p256::pkcs8::EncodePrivateKey as _;
use str0m_proto::crypto::dtls::DtlsCert;
use x509_cert::builder::profile::BuilderProfile;
use x509_cert::builder::{Builder as _, CertificateBuilder};
use x509_cert::certificate::TbsCertificate;
use x509_cert::der::Encode as _;
use x509_cert::ext::Extension;
use x509_cert::name::Name;
use x509_cert::serial_number::SerialNumber;
use x509_cert::spki::{SubjectPublicKeyInfo, SubjectPublicKeyInfoRef};
use x509_cert::time::Validity;

/// How long a certificate is good for. A daemon makes one when it
/// starts, and none runs as long.
const GOOD_FOR: Duration = Duration::from_hours(365 * 24);

/// A certificate and the key it is of.
///
/// Nobody signed it but itself. What a browser holds it to is its
/// fingerprint, which the answer to its offer says: the answer came
/// over a connection the browser trusts, and the handshake has to show
/// the certificate that was said there.
#[derive(Clone)]
pub(crate) struct Certificate {
    /// The certificate, in DER.
    certificate: Vec<u8>,
    /// Its key, in DER as PKCS#8.
    private_key: Vec<u8>,
}

impl core::fmt::Debug for Certificate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // One of the two is a key.
        f.debug_struct("Certificate").finish_non_exhaustive()
    }
}

impl Certificate {
    /// The certificate, in DER. It is no secret: every handshake shows
    /// it.
    #[cfg(test)]
    pub(crate) fn der(&self) -> &[u8] {
        &self.certificate
    }

    /// As `str0m` takes it.
    pub(crate) fn into_dtls(self) -> DtlsCert {
        DtlsCert {
            certificate: self.certificate,
            private_key: self.private_key,
        }
    }
}

/// A certificate that says who it is of, signs itself and says nothing
/// more.
struct Plain(Name);

impl BuilderProfile for Plain {
    fn get_issuer(&self, subject: &Name) -> Name {
        subject.clone()
    }

    fn get_subject(&self) -> Name {
        self.0.clone()
    }

    fn build_extensions(
        &self,
        _spk: SubjectPublicKeyInfoRef<'_>,
        _issuer_spk: SubjectPublicKeyInfoRef<'_>,
        _tbs: &TbsCertificate,
    ) -> x509_cert::builder::Result<Vec<Extension>> {
        Ok(Vec::new())
    }
}

/// Makes a certificate: a new key on the curve P-256, signed with
/// itself and SHA-256, good for a year. `None` when the system gives no
/// entropy.
///
/// It takes a few hundred microseconds, which is why a daemon makes one
/// and shows it on every call.
#[must_use]
pub(crate) fn certificate() -> Option<Certificate> {
    let key = SigningKey::generate();
    let subject = Name::from_str("CN=waveport").ok()?;
    // A browser refuses a certificate whose number it has seen on
    // another, from whatever machine: so the number is 16 bytes of
    // chance, kept above nought.
    let mut serial = [0_u8; 16];
    getrandom::fill(&mut serial).ok()?;
    if let Some(first) = serial.first_mut() {
        *first = (*first & 0x7f) | 0x01;
    }
    let serial = SerialNumber::new(&serial).ok()?;
    let validity = Validity::from_now(GOOD_FOR).ok()?;
    let public = SubjectPublicKeyInfo::from_key(key.verifying_key()).ok()?;
    let builder = CertificateBuilder::new(Plain(subject), serial, validity, public).ok()?;
    let certificate = builder.build::<_, DerSignature>(&key).ok()?;
    Some(Certificate {
        certificate: certificate.to_der().ok()?,
        private_key: key.to_pkcs8_der().ok()?.as_bytes().to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use x509_cert::der::Decode as _;

    use super::certificate;

    #[test]
    fn a_certificate_is_its_own_and_no_two_are_alike() {
        let one = certificate().unwrap();
        let other = certificate().unwrap();
        assert_ne!(one.der(), other.der());
        let read = x509_cert::Certificate::from_der(one.der()).unwrap();
        let inside = read.tbs_certificate();
        assert_eq!(inside.subject(), inside.issuer());
        assert_eq!(inside.subject().to_string(), "CN=waveport");
        // Each has a number of its own, which is above nought.
        let other = x509_cert::Certificate::from_der(other.der()).unwrap();
        let (a, b) = (
            inside.serial_number().as_bytes(),
            other.tbs_certificate().serial_number().as_bytes(),
        );
        assert_ne!(a, b);
        assert!(a.first().is_some_and(|first| first & 0x80 == 0));
        // What is printed of one holds no key.
        assert_eq!(format!("{one:?}"), "Certificate { .. }");
    }
}
