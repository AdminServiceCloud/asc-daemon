//! Certificate facts the web server needs (DMN-124): expiry, issuer and
//! names of a PEM chain, and whether a private key belongs to it.

use anyhow::{Context, Result, bail};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use x509_parser::prelude::{FromDer, GeneralName, X509Certificate};

/// What the API reports about a certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertInfo {
    pub not_before: i64,
    pub not_after: i64,
    /// The issuer's organization (or common name), e.g. "Let's Encrypt".
    pub issuer: String,
    /// DNS subject alternative names of the leaf.
    pub names: Vec<String>,
}

fn chain(pem: &str) -> Result<Vec<CertificateDer<'static>>> {
    let certs = CertificateDer::pem_slice_iter(pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .context("the certificate is not valid PEM")?;
    if certs.is_empty() {
        bail!("no certificate found in the PEM text");
    }
    Ok(certs)
}

/// Facts about the leaf (first) certificate of a PEM chain.
pub fn inspect(pem: &str) -> Result<CertInfo> {
    let certs = chain(pem)?;
    let (_, leaf) = X509Certificate::from_der(certs[0].as_ref())
        .map_err(|e| anyhow::anyhow!("cannot parse the certificate: {e}"))?;
    let issuer = leaf
        .issuer()
        .iter_organization()
        .chain(leaf.issuer().iter_common_name())
        .find_map(|attr| attr.as_str().ok().map(str::to_string))
        .unwrap_or_default();
    let mut names = Vec::new();
    if let Ok(Some(san)) = leaf.subject_alternative_name() {
        for name in &san.value.general_names {
            if let GeneralName::DNSName(dns) = name {
                names.push(dns.to_ascii_lowercase());
            }
        }
    }
    Ok(CertInfo {
        not_before: leaf.validity().not_before.timestamp(),
        not_after: leaf.validity().not_after.timestamp(),
        issuer,
        names,
    })
}

/// Fails unless `key_pem` is the private key of the chain's leaf.
pub fn check_pair(cert_pem: &str, key_pem: &str) -> Result<()> {
    let certs = chain(cert_pem)?;
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
        .context("the private key is not valid PEM (PKCS#8, PKCS#1 or SEC1)")?;
    let provider = rustls::crypto::ring::default_provider();
    rustls::sign::CertifiedKey::from_der(certs, key, &provider)
        .map_err(|e| anyhow::anyhow!("the private key does not match the certificate: {e}"))?;
    Ok(())
}

/// Whether `pattern` (a SAN, possibly `*.example.com`) covers `name`.
pub fn covers(pattern: &str, name: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(suffix) => name
            .split_once('.')
            .is_some_and(|(label, rest)| !label.is_empty() && rest == suffix),
        None => pattern == name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn self_signed(names: &[&str]) -> (String, String) {
        let key = rcgen::KeyPair::generate().unwrap();
        let params =
            rcgen::CertificateParams::new(names.iter().map(|n| n.to_string()).collect::<Vec<_>>())
                .unwrap();
        let cert = params.self_signed(&key).unwrap();
        (cert.pem(), key.serialize_pem())
    }

    #[test]
    fn inspects_names_and_validity() {
        let (cert, _) = self_signed(&["a.example.com", "*.example.com"]);
        let info = inspect(&cert).unwrap();
        assert_eq!(info.names, vec!["a.example.com", "*.example.com"]);
        assert!(info.not_after > info.not_before);
    }

    #[test]
    fn pairs_are_checked() {
        let (cert, key) = self_signed(&["a.example.com"]);
        let (_, other) = self_signed(&["a.example.com"]);
        check_pair(&cert, &key).unwrap();
        assert!(check_pair(&cert, &other).is_err());
        assert!(check_pair("junk", &key).is_err());
    }

    #[test]
    fn wildcard_coverage() {
        assert!(covers("*.example.com", "a.example.com"));
        assert!(!covers("*.example.com", "a.b.example.com"));
        assert!(!covers("*.example.com", "example.com"));
        assert!(covers("example.com", "example.com"));
    }
}
