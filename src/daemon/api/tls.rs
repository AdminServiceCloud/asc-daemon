//! TLS for the daemon API (DMN-061).
//!
//! The API listens on loopback by default and the platform reaches it through
//! an SSH tunnel. A node can instead be reached directly, and then the port is
//! exposed to the network — at which point the bearer token, which grants full
//! control of the machine, must never travel in the clear.
//!
//! The certificate is self-signed: nodes have no domain of their own, and
//! requiring one would make direct access a privilege of DNS owners. The
//! platform pins the certificate's fingerprint at registration, exactly as it
//! already pins the SSH host key, so a self-signed certificate is verified
//! rather than trusted blindly.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use sha2::{Digest, Sha256};
use tracing::info;

use crate::daemon::config::{Config, TlsMode};

/// Certificate and key paths, kept next to config.toml.
pub fn cert_path() -> PathBuf {
    Config::path().with_file_name("api.crt")
}

pub fn key_path() -> PathBuf {
    Config::path().with_file_name("api.key")
}

/// A loaded certificate together with the fingerprint the platform pins.
pub struct Materials {
    pub config: Arc<ServerConfig>,
    pub fingerprint: String,
}

/// SHA-256 of the certificate in DER form, formatted like an SSH fingerprint
/// so both look the same in the panel.
pub fn fingerprint(certificate: &CertificateDer<'_>) -> String {
    let digest = Sha256::digest(certificate.as_ref());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    format!("SHA256:{hex}")
}

/// Reads the current certificate's fingerprint without starting a server.
/// Used when reporting the node to the platform.
pub fn current_fingerprint() -> Option<String> {
    let certificates = CertificateDer::pem_file_iter(cert_path())
        .ok()?
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    certificates.first().map(fingerprint)
}

/// Prepares TLS material according to the configured mode, generating a
/// self-signed certificate on first use.
pub fn prepare(config: &Config) -> Result<Option<Materials>> {
    let (certificate_file, key_file) = match config.api.tls {
        TlsMode::Off => return Ok(None),
        TlsMode::SelfSigned => {
            ensure_self_signed(config)?;
            (cert_path(), key_path())
        }
        TlsMode::Files => {
            let certificate = config
                .api
                .tls_cert
                .clone()
                .context("api.tls = \"files\" requires api.tls_cert")?;
            let key = config
                .api
                .tls_key
                .clone()
                .context("api.tls = \"files\" requires api.tls_key")?;
            (certificate, key)
        }
    };

    let certificates: Vec<CertificateDer<'static>> =
        CertificateDer::pem_file_iter(&certificate_file)
            .with_context(|| format!("cannot read certificate {}", certificate_file.display()))?
            .collect::<Result<Vec<_>, _>>()
            .with_context(|| format!("cannot parse certificate {}", certificate_file.display()))?;
    if certificates.is_empty() {
        bail!("{} contains no certificate", certificate_file.display());
    }
    let key = PrivateKeyDer::from_pem_file(&key_file)
        .with_context(|| format!("cannot read private key {}", key_file.display()))?;

    let print = fingerprint(&certificates[0]);
    let mut server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .context("certificate and key do not match")?;
    // gRPC needs h2; REST and the WebSocket console stay on HTTP/1.1.
    server_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok(Some(Materials {
        config: Arc::new(server_config),
        fingerprint: print,
    }))
}

/// Issues a self-signed certificate if none exists yet. The SANs cover
/// everything the platform might dial the node by.
fn ensure_self_signed(config: &Config) -> Result<()> {
    if cert_path().exists() && key_path().exists() {
        return Ok(());
    }

    let mut names: Vec<String> = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    if let Some(host) = hostname() {
        names.push(host);
    }
    names.extend(config.api.tls_sans.iter().cloned());
    names.dedup();

    let certified = rcgen::generate_simple_self_signed(names.clone())
        .context("cannot generate a self-signed certificate")?;
    write_secret(&cert_path(), certified.cert.pem().as_bytes(), 0o644)?;
    write_secret(
        &key_path(),
        certified.signing_key.serialize_pem().as_bytes(),
        0o600,
    )?;
    info!(names = ?names, file = %cert_path().display(), "generated a self-signed API certificate");
    Ok(())
}

fn hostname() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn write_secret(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("cannot create directory {}", dir.display()))?;
    }
    std::fs::write(path, bytes).with_context(|| format!("cannot write {}", path.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("cannot set permissions on {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_are_stable_and_prefixed() {
        let certificate = CertificateDer::from(vec![1, 2, 3, 4]);
        let print = fingerprint(&certificate);
        assert!(print.starts_with("SHA256:"));
        assert_eq!(print.len(), "SHA256:".len() + 64);
        assert_eq!(print, fingerprint(&certificate));
        assert_ne!(print, fingerprint(&CertificateDer::from(vec![4, 3, 2, 1])));
    }
}
