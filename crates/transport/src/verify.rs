//! Certificate verifiers that pin device identities instead of trusting a CA (ADR-0004).
//! The handshake signature is still verified, which proves possession of the key whose
//! SPKI hash is the device id.

use std::sync::Arc;

use bridge_protocol::DeviceId;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms, verify_tls13_signature};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{CertificateError, DigitallySignedStruct, DistinguishedName, Error, SignatureScheme};
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};

use crate::identity::device_id_from_cert;

/// Decides which unknown devices may complete a TLS handshake with us.
pub trait TrustPolicy: Send + Sync + 'static {
    fn is_trusted(&self, id: &DeviceId) -> bool;

    /// While a pairing window is open, unknown devices are let through TLS; the core
    /// then allows them nothing but the pairing exchange.
    fn pairing_open(&self) -> bool {
        false
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExpectedPeer {
    /// Only this device may answer.
    Device(DeviceId),
    /// Any device (pairing by code: the PAKE binds the identity afterwards).
    Any,
}

fn algorithms(provider: &CryptoProvider) -> WebPkiSupportedAlgorithms {
    provider.signature_verification_algorithms
}

fn reject() -> Error {
    Error::InvalidCertificate(CertificateError::ApplicationVerificationFailure)
}

#[derive(Debug)]
pub(crate) struct PinnedServerVerifier {
    expect: ExpectedPeer,
    algs: WebPkiSupportedAlgorithms,
}

impl PinnedServerVerifier {
    pub(crate) fn new(expect: ExpectedPeer, provider: &CryptoProvider) -> Arc<Self> {
        Arc::new(Self {
            expect,
            algs: algorithms(provider),
        })
    }
}

impl ServerCertVerifier for PinnedServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        let id = device_id_from_cert(end_entity)
            .ok_or(Error::InvalidCertificate(CertificateError::BadEncoding))?;
        match self.expect {
            ExpectedPeer::Device(want) if want != id => Err(reject()),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls13RequiredForQuic,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

pub(crate) struct PinnedClientVerifier {
    trust: Arc<dyn TrustPolicy>,
    algs: WebPkiSupportedAlgorithms,
}

impl std::fmt::Debug for PinnedClientVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PinnedClientVerifier")
    }
}

impl PinnedClientVerifier {
    pub(crate) fn new(trust: Arc<dyn TrustPolicy>, provider: &CryptoProvider) -> Arc<Self> {
        Arc::new(Self {
            trust,
            algs: algorithms(provider),
        })
    }
}

impl ClientCertVerifier for PinnedClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        let id = device_id_from_cert(end_entity)
            .ok_or(Error::InvalidCertificate(CertificateError::BadEncoding))?;
        if self.trust.is_trusted(&id) || self.trust.pairing_open() {
            Ok(ClientCertVerified::assertion())
        } else {
            Err(reject())
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls13RequiredForQuic,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}
