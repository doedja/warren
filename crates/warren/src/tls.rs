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
use tokio_rustls::rustls::crypto::CryptoProvider;
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

/// Like [`server_acceptor`] but persists the cert/key under `dir` and reuses
/// them on restart, so the fingerprint stays stable across redeploys.
pub fn server_acceptor_from_dir(dir: &str) -> Result<(TlsAcceptor, String)> {
    init_crypto();
    let cert_path = std::path::Path::new(dir).join("warren-cert.der");
    let key_path = std::path::Path::new(dir).join("warren-key.der");

    let (cert_bytes, key_bytes) = if cert_path.exists() && key_path.exists() {
        (std::fs::read(&cert_path)?, std::fs::read(&key_path)?)
    } else {
        let cert = rcgen::generate_simple_self_signed(vec!["warren".to_string()])
            .map_err(|e| anyhow!("self-signed cert: {e}"))?;
        let c = cert.cert.der().to_vec();
        let k = cert.key_pair.serialize_der();
        std::fs::create_dir_all(dir).ok();
        // The private key must not be world-readable, not even between create
        // and a later chmod: set the mode at create time (identity.rs does the
        // same). Restrict the cert too for good measure.
        write_private(&cert_path, &c)?;
        write_private(&key_path, &k)?;
        (c, k)
    };

    let fingerprint = hex::encode(Sha256::digest(&cert_bytes));
    let cert_der = CertificateDer::from(cert_bytes);
    let key_der = PrivatePkcs8KeyDer::from(key_bytes);
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], PrivateKeyDer::Pkcs8(key_der))
        .map_err(|e| anyhow!("server tls config: {e}"))?;
    Ok((TlsAcceptor::from(Arc::new(config)), fingerprint))
}

/// Write `bytes` to `path` with owner-only permissions from the moment the
/// file exists (no world-readable window).
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)?.write_all(bytes)
}

/// Build a TLS connector. With `fingerprint` set, the hub cert must match it.
/// With `insecure` true, any cert is accepted (dev only).
pub fn client_connector(fingerprint: Option<String>, insecure: bool) -> Result<TlsConnector> {
    init_crypto();
    let provider = CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(tokio_rustls::rustls::crypto::ring::default_provider()));
    let verifier: Arc<dyn ServerCertVerifier> = if insecure {
        Arc::new(AcceptAny { provider })
    } else {
        let hex_pin = fingerprint
            .ok_or_else(|| anyhow!("--tls requires --hub-fingerprint <sha256> (or --insecure)"))?;
        let pin = hex::decode(hex_pin.trim()).map_err(|e| anyhow!("bad fingerprint hex: {e}"))?;
        if pin.len() != 32 {
            return Err(anyhow!("fingerprint must be 32 bytes of SHA256 hex"));
        }
        Arc::new(PinnedCert { pin, provider })
    };
    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

/// Verifier that trusts a cert iff its SHA256 matches the pinned value.
///
/// Matching the fingerprint alone is NOT enough: the handshake signature
/// (CertificateVerify) is the only proof the peer holds the cert's private
/// key. The cert itself is public (served to anyone who connects), so both
/// signature callbacks delegate to the real crypto provider instead of
/// blind-accepting.
#[derive(Debug)]
struct PinnedCert {
    pin: Vec<u8>,
    provider: Arc<CryptoProvider>,
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
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        tokio_rustls::rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        tokio_rustls::rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Dev-only verifier that accepts any certificate (still checks the peer holds
/// the presented cert's key, it just doesn't care which cert it is).
#[derive(Debug)]
struct AcceptAny {
    provider: Arc<CryptoProvider>,
}

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
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        tokio_rustls::rustls::crypto::verify_tls12_signature(
            m,
            c,
            d,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        tokio_rustls::rustls::crypto::verify_tls13_signature(
            m,
            c,
            d,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
