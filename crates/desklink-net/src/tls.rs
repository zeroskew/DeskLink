//! TLS setup: mutual authentication with self-signed certificates.
//!
//! The handshake proves each side holds the private key for the certificate
//! it presents; *which* certificates are acceptable is decided afterwards by
//! fingerprint pinning (see `lib.rs`), not by a CA.

use crate::identity::Identity;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use std::sync::Arc;
use std::time::Duration;

pub const ALPN: &[u8] = b"desklink/1";
pub const SERVER_NAME: &str = "desklink";

#[derive(Debug)]
struct PinLater(Arc<CryptoProvider>);

impl PinLater {
    fn tls12(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct)
        -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn tls13(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct)
        -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

impl ServerCertVerifier for PinLater {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct)
        -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls12(m, c, d)
    }
    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct)
        -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls13(m, c, d)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes()
    }
}

impl ClientCertVerifier for PinLater {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }
    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct)
        -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls12(m, c, d)
    }
    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct)
        -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls13(m, c, d)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes()
    }
}

fn transport(idle_timeout: Duration) -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    t.keep_alive_interval(Some(idle_timeout / 4));
    t.max_idle_timeout(Some(idle_timeout.try_into().expect("idle timeout")));
    // LAN: start with a realistic RTT so loss recovery reacts in milliseconds.
    t.initial_rtt(Duration::from_millis(10));
    Arc::new(t)
}

pub fn configs(
    id: &Identity,
    idle_timeout: Duration,
) -> Result<(quinn::ServerConfig, quinn::ClientConfig), String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(PinLater(provider.clone()));
    let e = |e: rustls::Error| e.to_string();

    let mut server = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(e)?
        .with_client_cert_verifier(verifier.clone())
        .with_single_cert(vec![id.cert.clone()], id.key())
        .map_err(e)?;
    server.alpn_protocols = vec![ALPN.to_vec()];

    let mut client = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(e)?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![id.cert.clone()], id.key())
        .map_err(e)?;
    client.alpn_protocols = vec![ALPN.to_vec()];

    let qs = quinn::crypto::rustls::QuicServerConfig::try_from(server).map_err(|e| e.to_string())?;
    let qc = quinn::crypto::rustls::QuicClientConfig::try_from(client).map_err(|e| e.to_string())?;
    let mut sc = quinn::ServerConfig::with_crypto(Arc::new(qs));
    sc.transport_config(transport(idle_timeout));
    let mut cc = quinn::ClientConfig::new(Arc::new(qc));
    cc.transport_config(transport(idle_timeout));
    Ok((sc, cc))
}
