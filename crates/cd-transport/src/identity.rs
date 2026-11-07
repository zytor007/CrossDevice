use std::fs;
use std::path::{Path, PathBuf};

use rcgen::{generate_simple_self_signed, CertifiedKey};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

use crate::error::TransportError;

const CERT_FILE: &str = "identity.cert.der";
const KEY_FILE: &str = "identity.key.der";

/// Self-signed device identity. After pairing the SHA-256 fingerprint is pinned
/// by both peers; TLS 1.3 still carries every byte.
pub struct Identity {
    name: String,
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    fingerprint: [u8; 32],
}

impl Identity {
    pub fn generate(name: impl Into<String>) -> Result<Self, TransportError> {
        let name = name.into();
        let CertifiedKey { cert, key_pair } = generate_simple_self_signed(vec![name.clone()])
            .map_err(|error| TransportError::Certificate(error.to_string()))?;
        let cert = cert.der().clone();
        let fingerprint: [u8; 32] = Sha256::digest(cert.as_ref()).into();
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));
        Ok(Self {
            name,
            cert,
            key,
            fingerprint,
        })
    }

    /// Loads a stable identity from `directory`, generating it once on first use.
    /// The private key is written with owner-only permissions on Unix.
    pub fn load_or_generate(
        directory: impl AsRef<Path>,
        name: impl Into<String>,
    ) -> Result<Self, TransportError> {
        let directory = directory.as_ref();
        let cert_path = directory.join(CERT_FILE);
        let key_path = directory.join(KEY_FILE);
        let name = name.into();

        match (cert_path.exists(), key_path.exists()) {
            (true, true) => {
                let cert_bytes = fs::read(&cert_path)
                    .map_err(|error| TransportError::Certificate(error.to_string()))?;
                let key_bytes = fs::read(&key_path)
                    .map_err(|error| TransportError::Certificate(error.to_string()))?;
                Self::from_der(name, cert_bytes, key_bytes)
            }
            (false, false) => {
                let identity = Self::generate(name)?;
                fs::create_dir_all(directory)
                    .map_err(|error| TransportError::Certificate(error.to_string()))?;
                fs::write(&cert_path, identity.cert.as_ref())
                    .map_err(|error| TransportError::Certificate(error.to_string()))?;
                fs::write(&key_path, identity.key.secret_der())
                    .map_err(|error| TransportError::Certificate(error.to_string()))?;
                restrict_key_permissions(&key_path)?;
                Ok(identity)
            }
            _ => Err(TransportError::Certificate(format!(
                "incomplete identity in {}: cert={}, key={}",
                directory.display(),
                cert_path.exists(),
                key_path.exists()
            ))),
        }
    }

    pub fn from_der(
        name: impl Into<String>,
        cert: Vec<u8>,
        key: Vec<u8>,
    ) -> Result<Self, TransportError> {
        if cert.is_empty() || key.is_empty() {
            return Err(TransportError::Certificate(
                "certificate and key must not be empty".to_owned(),
            ));
        }
        let cert = CertificateDer::from(cert);
        let fingerprint: [u8; 32] = Sha256::digest(cert.as_ref()).into();
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key));
        Ok(Self {
            name: name.into(),
            cert,
            key,
            fingerprint,
        })
    }

    pub fn path_name(directory: impl AsRef<Path>) -> PathBuf {
        directory.as_ref().join(CERT_FILE)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn cert(&self) -> &CertificateDer<'static> {
        &self.cert
    }

    pub fn clone_key(&self) -> PrivateKeyDer<'static> {
        self.key.clone_key()
    }

    pub fn fingerprint(&self) -> &[u8; 32] {
        &self.fingerprint
    }

    pub fn fingerprint_hex(&self) -> String {
        self.fingerprint
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

#[cfg(unix)]
fn restrict_key_permissions(path: &Path) -> Result<(), TransportError> {
    use std::os::unix::fs::PermissionsExt;
    let permissions = fs::Permissions::from_mode(0o600);
    fs::set_permissions(path, permissions)
        .map_err(|error| TransportError::Certificate(error.to_string()))
}

#[cfg(not(unix))]
fn restrict_key_permissions(_path: &Path) -> Result<(), TransportError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_has_a_stable_sha256_fingerprint() {
        let identity = Identity::generate("crossdev.local").expect("identity");
        assert_eq!(identity.fingerprint_hex().len(), 64);
        assert_eq!(identity.fingerprint().len(), 32);
        assert!(!identity.cert().as_ref().is_empty());
        assert_eq!(identity.name(), "crossdev.local");
    }

    #[test]
    fn load_or_generate_is_stable_across_restarts() {
        let directory = std::env::temp_dir().join(format!(
            "crossdev-identity-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let first = Identity::load_or_generate(&directory, "crossdev.local").expect("first");
        let second = Identity::load_or_generate(&directory, "crossdev.local").expect("second");
        assert_eq!(first.fingerprint_hex(), second.fingerprint_hex());
        assert!(directory.join(CERT_FILE).exists());
        assert!(directory.join(KEY_FILE).exists());
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn incomplete_identity_is_rejected() {
        let directory = std::env::temp_dir().join(format!(
            "crossdev-identity-broken-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join(CERT_FILE), b"only-cert").unwrap();
        assert!(Identity::load_or_generate(&directory, "crossdev.local").is_err());
        let _ = fs::remove_dir_all(&directory);
    }
}
