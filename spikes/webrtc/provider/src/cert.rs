//! A self-signed certificate for DTLS, made without `rcgen`.

use std::str::FromStr;
use std::time::Duration;

use p256::ecdsa::{DerSignature, SigningKey};
use p256::elliptic_curve::Generate;
use p256::pkcs8::EncodePrivateKey;
use x509_cert::builder::profile::BuilderProfile;
use x509_cert::builder::{Builder, CertificateBuilder};
use x509_cert::certificate::TbsCertificate;
use x509_cert::der::Encode;
use x509_cert::ext::Extension;
use x509_cert::name::Name;
use x509_cert::serial_number::SerialNumber;
use x509_cert::spki::{SubjectPublicKeyInfo, SubjectPublicKeyInfoRef};
use x509_cert::time::Validity;

/// A certificate and its key, both DER: the key as PKCS#8, which is what
/// dimpl's RustCrypto backend loads.
pub struct SelfSigned {
    pub certificate: Vec<u8>,
    pub private_key: Vec<u8>,
}

/// Self-signed, the subject is its own issuer, no extensions: what `rcgen`
/// produces for dimpl, less the two extensions `rcgen` adds by default.
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

/// Makes a certificate. `None` when the system gives no randomness or the
/// encoding fails, which str0m treats as "no certificate".
pub fn generate() -> Option<SelfSigned> {
    let key = SigningKey::generate();
    let subject = Name::from_str("CN=DTLS Peer,O=DTLS").ok()?;

    // Firefox refuses two certificates with one serial number, across
    // processes and machines, so the serial is random: 16 bytes, the first
    // kept positive and non-zero.
    let mut serial = [0u8; 16];
    getrandom::fill(&mut serial).ok()?;
    serial[0] = (serial[0] & 0x7f) | 0x01;
    let serial = SerialNumber::new(&serial).ok()?;

    let validity = Validity::from_now(Duration::from_secs(365 * 24 * 3600)).ok()?;
    let spki = SubjectPublicKeyInfo::from_key(key.verifying_key()).ok()?;
    let builder = CertificateBuilder::new(Plain(subject), serial, validity, spki).ok()?;
    let certificate = builder.build::<_, DerSignature>(&key).ok()?;

    Some(SelfSigned {
        certificate: certificate.to_der().ok()?,
        private_key: key.to_pkcs8_der().ok()?.as_bytes().to_vec(),
    })
}
