//! Machine identity: a self-signed certificate whose SHA-256 fingerprint is
//! pinned by peers on first contact (later: confirmed with a pairing code).

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};
use std::path::Path;

#[derive(Clone)]
pub struct Identity {
    pub cert: CertificateDer<'static>,
    key: Vec<u8>,
}

impl Identity {
    pub fn generate() -> Result<Self, String> {
        let ck = rcgen::generate_simple_self_signed(vec!["desklink".to_string()])
            .map_err(|e| e.to_string())?;
        Ok(Identity {
            cert: ck.cert.der().clone(),
            key: ck.signing_key.serialize_der(),
        })
    }

    /// Loads `cert.der`/`key.der` from `dir`, creating them on first run.
    pub fn load_or_create(dir: &Path) -> Result<Self, String> {
        Self::load_or_create_at(&dir.join("cert.der"), &dir.join("key.der"))
    }

    /// Loads the certificate and key from these files, creating both on first
    /// run.
    pub fn load_or_create_at(cert_path: &Path, key_path: &Path) -> Result<Self, String> {
        if let (Ok(cert), Ok(key)) = (std::fs::read(cert_path), std::fs::read(key_path)) {
            return Ok(Identity { cert: CertificateDer::from(cert), key });
        }
        let id = Self::generate()?;
        if let Some(dir) = cert_path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(cert_path, id.cert.as_ref()).map_err(|e| e.to_string())?;
        std::fs::write(key_path, &id.key).map_err(|e| e.to_string())?;
        Ok(id)
    }

    pub fn key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key.clone()))
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.cert)
    }
}

pub fn fingerprint(cert: &CertificateDer<'_>) -> String {
    Sha256::digest(cert.as_ref()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Short human-readable form for logs and UI: `ab12-cd34-ef56-7890`.
pub fn short(fp: &str) -> String {
    fp.as_bytes()
        .chunks(4)
        .take(4)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("-")
}
