//! GitHub Releases client: release lookup per channel, asset download with
//! mandatory SHA-256 verification.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use asc_daemon::daemon::config::Channel;
use asc_daemon::daemon::http;

pub const REPO: &str = "AdminServiceCloud/asc-daemon";
const API: &str = "https://api.github.com";

#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    /// Release tag, e.g. `v0.2.0`.
    pub tag_name: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
}

impl Release {
    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

/// Latest release of the channel: `stable` — the latest non-prerelease,
/// `beta` — the newest release including prereleases.
pub fn latest_release(channel: Channel) -> Result<Release> {
    match channel {
        Channel::Stable => {
            let url = format!("{API}/repos/{REPO}/releases/latest");
            let release: Release = get_json(&url)?;
            Ok(release)
        }
        Channel::Beta => {
            let url = format!("{API}/repos/{REPO}/releases?per_page=10");
            let releases: Vec<Release> = get_json(&url)?;
            releases
                .into_iter()
                .find(|r| !r.draft)
                .context("no releases published yet")
        }
    }
}

fn get_json<T: serde::de::DeserializeOwned>(url: &str) -> Result<T> {
    let raw = http::get_string(url)?;
    serde_json::from_str(&raw).with_context(|| format!("unexpected response from {url}"))
}

/// Download an asset into memory (release artifacts are small).
pub fn download(asset: &Asset) -> Result<Vec<u8>> {
    http::get_bytes(&asset.browser_download_url)
        .with_context(|| format!("cannot download {}", asset.name))
}

/// Verify `data` against `sha256sum` output (`<hex>  <path>`); the recorded
/// path is ignored — only the hash matters.
pub fn verify_sha256(data: &[u8], checksum_file: &str, what: &str) -> Result<()> {
    let expected = checksum_file
        .split_whitespace()
        .next()
        .context("empty checksum file")?
        .to_lowercase();
    let actual = hex(&Sha256::digest(data));
    if actual != expected {
        bail!("checksum mismatch for {what}: expected {expected}, got {actual}");
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_verification() {
        // sha256("hello") — well-known vector.
        let sum = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824  dist/x.tar.gz";
        verify_sha256(b"hello", sum, "x.tar.gz").unwrap();
        assert!(verify_sha256(b"hello!", sum, "x.tar.gz").is_err());
        assert!(verify_sha256(b"hello", "", "x").is_err());
    }

    #[test]
    fn asset_lookup() {
        let release = Release {
            tag_name: "v1.0.0".into(),
            draft: false,
            assets: vec![Asset {
                name: "asc-v1.0.0-x86_64-unknown-linux-gnu.tar.gz".into(),
                browser_download_url: "https://example.com/a".into(),
            }],
        };
        assert!(
            release
                .asset("asc-v1.0.0-x86_64-unknown-linux-gnu.tar.gz")
                .is_some()
        );
        assert!(release.asset("missing").is_none());
    }
}
