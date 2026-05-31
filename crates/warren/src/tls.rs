//! TLS for the node-hub link (opt-in via `--tls`).
//!
//! The hub generates a self-signed cert at startup and prints its SHA256
//! fingerprint. A node pins that fingerprint (`--hub-fingerprint`), so trust
//! does not depend on a CA. `--insecure` accepts any cert (dev only).

use std::sync::Arc;

use anyhow::{anyhow, Result};
use sha2::{Digest, Sha256};
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime,
};
use tokio_rustls::rustls::{ClientConfig, DigitallySignedStruct, ServerConfig, SignatureScheme};
use tokio_rustls::{TlsAcceptor, TlsConnector};

/// Install the ring crypto provider once. Idempotent.
pub fn init_crypto() {
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
}

/// Generate a self-signed cert and return (acceptor, hex SHA256 fingerprint).
pub fn server_acceptor() -> Result<(TlsAcceptor, String)> {
    init_crypto();
    let cert = rcgen::generate_simple_self_signed(vec!["warren".to_string()])
        .map_err(|e| anyhow!("self-signed cert: {e}"))?;
    let cert_der: CertificateDer<'static> = cert.cert.der().clone();
    let fingerprint = hex::encode(Sha256::digest(cert_der.as_ref()));

    let key_der = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], PrivateKeyDer::Pkcs8(key_der))
        .map_err(|e| anyhow!("server tls config: {e}"))?;
    Ok((TlsAcceptor::from(Arc::new(config)), fingerprint))
}

/// Build a TLS connector. With `fingerprint` set, the hub cert must match it.
/// With `insecure` true, any cert is accepted (dev only).
pub fn client_connector(fingerprint: Option<String>, insecure: bool) -> Result<TlsConnector> {
    init_crypto();
    let verifier: Arc<dyn ServerCertVerifier> = if insecure {
        Arc::new(AcceptAny)
    } else {
        let hex_pin = fingerprint
            .ok_or_else(|| anyhow!("--tls requires --hub-fingerprint <sha256> (or --insecure)"))?;
        let pin = hex::decode(hex_pin.trim()).map_err(|e| anyhow!("bad fingerprint hex: {e}"))?;
        if pin.len() != 32 {
            return Err(anyhow!("fingerprint must be 32 bytes of SHA256 hex"));
        }
        Arc::new(PinnedCert { pin })
    };
    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

fn all_schemes() -> Vec<SignatureScheme> {
    vec![
        SignatureScheme::RSA_PKCS1_SHA256,
        SignatureScheme::RSA_PKCS1_SHA384,
        SignatureScheme::RSA_PKCS1_SHA512,
        SignatureScheme::ECDSA_NISTP256_SHA256,
        SignatureScheme::ECDSA_NISTP384_SHA384,
        SignatureScheme::RSA_PSS_SHA256,
        SignatureScheme::RSA_PSS_SHA384,
        SignatureScheme::RSA_PSS_SHA512,
        SignatureScheme::ED25519,
    ]
}

/// Verifier that trusts a cert iff its SHA256 matches the pinned value.
#[derive(Debug)]
struct PinnedCert {
    pin: Vec<u8>,
}

impl ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, tokio_rustls::rustls::Error> {
        let got = Sha256::digest(end_entity.as_ref());
        if got.as_slice() == self.pin.as_slice() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(tokio_rustls::rustls::Error::General(
                "hub cert fingerprint mismatch".into(),
            ))
        }
    }
    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        all_schemes()
    }
}

/// Dev-only verifier that accepts any certificate.
#[derive(Debug)]
struct AcceptAny;

impl ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _e: &CertificateDer<'_>,
        _i: &[CertificateDer<'_>],
        _s: &ServerName<'_>,
        _o: &[u8],
        _n: UnixTime,
    ) -> Result<ServerCertVerified, tokio_rustls::rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _m: &[u8],
        _c: &CertificateDer<'_>,
        _d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _m: &[u8],
        _c: &CertificateDer<'_>,
        _d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        all_schemes()
    }
}
