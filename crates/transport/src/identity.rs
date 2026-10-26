//! Device identity: a self-signed ECDSA P-256 certificate whose SubjectPublicKeyInfo
//! hash is the device id (ADR-0004).

use std::fs;
use std::io;
use std::path::Path;

use bridge_protocol::DeviceId;
use rcgen::{
    CertificateParams, DistinguishedName, DnType, KeyPair, PKCS_ECDSA_P256_SHA256, PublicKeyData,
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use crate::TransportError;

pub struct DeviceIdentity {
    cert: CertificateDer<'static>,
    key_pkcs8: Vec<u8>,
    id: DeviceId,
}

impl std::fmt::Debug for DeviceIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceIdentity")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Clone for DeviceIdentity {
    fn clone(&self) -> Self {
        Self {
            cert: self.cert.clone(),
            key_pkcs8: self.key_pkcs8.clone(),
            id: self.id,
        }
    }
}

impl DeviceIdentity {
    pub fn generate() -> Result<Self, TransportError> {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(tls)?;
        // The certificate is only a container for the key; names are not checked.
        let mut params = CertificateParams::new(vec!["bridge".to_owned()]).map_err(tls)?;
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "Bridge device");
        params.distinguished_name = dn;
        let cert = params.self_signed(&key).map_err(tls)?;
        let id = device_id_from_spki(&key.subject_public_key_info());
        Ok(Self {
            cert: cert.der().clone(),
            key_pkcs8: key.serialize_der(),
            id,
        })
    }

    pub fn from_der(cert: Vec<u8>, key_pkcs8: Vec<u8>) -> Result<Self, TransportError> {
        let cert = CertificateDer::from(cert);
        let id = device_id_from_cert(&cert).ok_or(TransportError::BadIdentity("certificate"))?;
        let key = KeyPair::try_from(key_pkcs8.as_slice())
            .map_err(|_| TransportError::BadIdentity("key"))?;
        if device_id_from_spki(&key.subject_public_key_info()) != id {
            return Err(TransportError::BadIdentity(
                "key does not match certificate",
            ));
        }
        Ok(Self {
            cert,
            key_pkcs8,
            id,
        })
    }

    /// Loads `cert.der`/`key.der` from `dir`, creating them on first use. The key file
    /// is readable only by the owner on Unix; on Windows it inherits the ACL of the
    /// per-user data directory.
    pub fn load_or_create(dir: &Path) -> Result<Self, TransportError> {
        let cert_path = dir.join("cert.der");
        let key_path = dir.join("key.der");
        if cert_path.exists() && key_path.exists() {
            return Self::from_der(fs::read(&cert_path)?, fs::read(&key_path)?);
        }
        fs::create_dir_all(dir)?;
        let identity = Self::generate()?;
        write_private(&key_path, &identity.key_pkcs8)?;
        fs::write(&cert_path, identity.cert.as_ref())?;
        Ok(identity)
    }

    pub fn id(&self) -> DeviceId {
        self.id
    }

    pub fn cert(&self) -> CertificateDer<'static> {
        self.cert.clone()
    }

    pub fn key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_pkcs8.clone()))
    }
}

fn tls(e: rcgen::Error) -> TransportError {
    TransportError::Tls(e.to_string())
}

#[cfg(unix)]
fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(data)
}

#[cfg(not(unix))]
fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    fs::write(path, data)
}

pub fn device_id_from_spki(spki: &[u8]) -> DeviceId {
    let digest = ring::digest::digest(&ring::digest::SHA256, spki);
    let mut id = [0u8; 32];
    id.copy_from_slice(digest.as_ref());
    DeviceId(id)
}

pub fn device_id_from_cert(cert: &CertificateDer<'_>) -> Option<DeviceId> {
    spki_of(cert.as_ref()).map(device_id_from_spki)
}

/// Returns the DER of `tbsCertificate.subjectPublicKeyInfo` (RFC 5280 §4.1).
/// rustls-webpki does not expose it, and pulling in a full X.509 parser for one
/// field is not worth it.
pub(crate) fn spki_of(cert: &[u8]) -> Option<&[u8]> {
    let (cert_body, _) = der_element(cert, 0x30)?;
    let (tbs, _) = der_element(cert_body, 0x30)?;
    let mut rest = tbs;
    if rest.first() == Some(&0xa0) {
        rest = der_skip(rest)?;
    }
    for _ in 0..5 {
        // serialNumber, signature, issuer, validity, subject
        rest = der_skip(rest)?;
    }
    let len = der_total_len(rest)?;
    (rest.first() == Some(&0x30)).then(|| &rest[..len])
}

/// Parses `tag ‖ len ‖ value`, returning the value and the remaining input.
fn der_element(input: &[u8], tag: u8) -> Option<(&[u8], &[u8])> {
    if *input.first()? != tag {
        return None;
    }
    let (len, header) = der_len(&input[1..])?;
    let start = 1 + header;
    let end = start.checked_add(len)?;
    Some((input.get(start..end)?, &input[end..]))
}

fn der_skip(input: &[u8]) -> Option<&[u8]> {
    let total = der_total_len(input)?;
    input.get(total..)
}

fn der_total_len(input: &[u8]) -> Option<usize> {
    input.first()?;
    let (len, header) = der_len(input.get(1..)?)?;
    let total = (1 + header).checked_add(len)?;
    (total <= input.len()).then_some(total)
}

/// Definite-length DER length: returns (length, bytes used by the length field).
fn der_len(input: &[u8]) -> Option<(usize, usize)> {
    let first = *input.first()?;
    if first < 0x80 {
        return Some((usize::from(first), 1));
    }
    let n = usize::from(first & 0x7f);
    if n == 0 || n > 4 {
        return None;
    }
    let bytes = input.get(1..=n)?;
    let len = bytes
        .iter()
        .fold(0usize, |acc, b| (acc << 8) | usize::from(*b));
    Some((len, 1 + n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_identity_has_stable_spki_based_id() {
        let id = DeviceIdentity::generate().unwrap();
        assert_eq!(device_id_from_cert(&id.cert()), Some(id.id()));
        let again = DeviceIdentity::from_der(id.cert().to_vec(), id.key_pkcs8.clone()).unwrap();
        assert_eq!(again.id(), id.id());
    }

    #[test]
    fn mismatched_key_is_rejected() {
        let a = DeviceIdentity::generate().unwrap();
        let b = DeviceIdentity::generate().unwrap();
        assert_ne!(a.id(), b.id());
        assert!(DeviceIdentity::from_der(a.cert().to_vec(), b.key_pkcs8.clone()).is_err());
        assert!(DeviceIdentity::from_der(vec![1, 2, 3], a.key_pkcs8.clone()).is_err());
    }

    #[test]
    fn load_or_create_persists_identity() {
        let dir = std::env::temp_dir().join(format!("bridge-id-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let first = DeviceIdentity::load_or_create(&dir).unwrap();
        let second = DeviceIdentity::load_or_create(&dir).unwrap();
        assert_eq!(first.id(), second.id());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn der_parser_survives_garbage() {
        for input in [
            &[][..],
            &[0x30],
            &[0x30, 0x85, 1, 1, 1, 1, 1],
            &[0x30, 0x02, 0x30],
            &[0x31, 0x00],
        ] {
            assert!(spki_of(input).is_none());
        }
        let id = DeviceIdentity::generate().unwrap();
        let der = id.cert().to_vec();
        for cut in 0..der.len() {
            let _ = spki_of(&der[..cut]);
        }
        assert!(spki_of(&der).is_some());
    }

    #[test]
    fn der_lengths() {
        assert_eq!(der_len(&[0x05]), Some((5, 1)));
        assert_eq!(der_len(&[0x81, 0xc8]), Some((200, 2)));
        assert_eq!(der_len(&[0x82, 0x01, 0x00]), Some((256, 3)));
        assert_eq!(der_len(&[0x80]), None);
        assert_eq!(der_len(&[0x85, 0, 0, 0, 0, 0]), None);
    }
}
