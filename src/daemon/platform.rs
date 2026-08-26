//! Binding the node to an AdminService.Cloud platform (DMN-058).
//!
//! The platform issues a one-time registration token; `install.sh --token`,
//! `asc-updater install --token` and `asc connect` all funnel into
//! [`register`], which stores the token, remembers the platform URL and calls
//! the bootstrap endpoint once.
//!
//! There is no tunnel yet (that is NODE-002 on the platform side), so a
//! registered node is exactly that — registered. It does not report health and
//! the platform will not show it as online until the channel exists.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tracing::warn;

use crate::daemon::config::Config;
use crate::daemon::http;

/// Default platform, used when `--url` is omitted.
pub const DEFAULT_PLATFORM_URL: &str = "https://adminservice.cloud";

/// The registration token file, next to config.toml and readable by root only.
/// config.toml itself is world-readable, so secrets never go in it.
pub fn platform_token_path() -> PathBuf {
    Config::path().with_file_name("platform.token")
}

/// Reject anything that is not a plain token: the value ends up in a URL and
/// in a JSON body, and a permissive check here would be the wrong place to be
/// generous.
pub fn valid_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 128
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Normalise and validate a platform base URL.
pub fn normalize_url(url: &str) -> Result<String> {
    let trimmed = url.trim().trim_end_matches('/');
    if !trimmed.starts_with("https://") && !trimmed.starts_with("http://") {
        bail!("platform URL must start with https:// or http://");
    }
    if trimmed.len() > 512 || trimmed.contains(char::is_whitespace) {
        bail!("platform URL is not acceptable");
    }
    Ok(trimmed.to_string())
}

#[derive(Debug, Deserialize)]
struct RegisterResponse {
    #[serde(default, rename = "nodeId")]
    node_id: String,
    #[serde(default, rename = "organizationId")]
    organization_id: String,
}

/// Store the token and URL, then register with the platform.
///
/// Registration failure is reported but never fatal: the daemon is installed
/// and useful locally regardless of whether the platform is reachable right
/// now, and `asc connect` can retry later.
pub fn register(config: &mut Config, token: &str, url: Option<&str>) -> Result<Registration> {
    if !valid_token(token) {
        bail!("registration token contains unexpected characters");
    }
    let platform_url = match url {
        Some(value) => normalize_url(value)?,
        None => config
            .platform
            .url
            .clone()
            .unwrap_or_else(|| DEFAULT_PLATFORM_URL.to_string()),
    };

    write_secret(&platform_token_path(), token)?;
    config.platform.url = Some(platform_url.clone());
    config.save().context("cannot save config.toml")?;

    // The platform needs to know how to reach this node back: over its own
    // SSH connection, or straight to the API when it is exposed with TLS.
    let (api_endpoint, tls_fingerprint) = direct_endpoint(config);
    // Handed over only in direct mode: with SSH the platform reads the token
    // off the machine itself, and there is no reason to send it twice.
    let api_token = if api_endpoint.is_empty() {
        String::new()
    } else {
        std::fs::read_to_string(crate::daemon::api::api_token_path())
            .map(|value| value.trim().to_string())
            .unwrap_or_default()
    };
    let body = serde_json::json!({
        "token": token,
        "hostname": hostname(),
        "primaryIp": primary_ip().unwrap_or_default(),
        "os": os_description(),
        "arch": std::env::consts::ARCH,
        "daemonVersion": crate::VERSION,
        "apiEndpoint": api_endpoint,
        "tlsFingerprint": tls_fingerprint,
        "apiToken": api_token,
    })
    .to_string();

    let endpoint = format!("{platform_url}/bootstrap/asc.node.v1.BootstrapService/RegisterNode");
    let response = http::post_json(&endpoint, &body)?;
    let parsed: RegisterResponse = serde_json::from_str(&response)
        .with_context(|| format!("unexpected response from {endpoint}"))?;
    if parsed.node_id.is_empty() {
        bail!("platform did not return a node id");
    }

    config.platform.node_id = Some(parsed.node_id.clone());
    config.platform.registered_at = Some(now_rfc3339());
    config.save().context("cannot save config.toml")?;

    Ok(Registration {
        node_id: parsed.node_id,
        organization_id: parsed.organization_id,
        platform_url,
    })
}

/// Where the platform can dial this daemon directly, if anywhere. An API bound
/// to loopback is not reachable from outside, and one served without TLS must
/// not be advertised: the bearer token would cross the network in the clear.
fn direct_endpoint(config: &Config) -> (String, String) {
    use crate::daemon::api::tls;
    use crate::daemon::config::TlsMode;

    if config.api.tls == TlsMode::Off {
        return (String::new(), String::new());
    }
    let listen = config.api.listen.clone();
    if listen.starts_with("127.") || listen.starts_with("localhost") {
        return (String::new(), String::new());
    }
    let port = listen.rsplit(':').next().unwrap_or("8420").to_string();
    let host = primary_ip().unwrap_or_default();
    if host.is_empty() {
        return (String::new(), String::new());
    }
    let fingerprint = tls::current_fingerprint().unwrap_or_default();
    if fingerprint.is_empty() {
        return (String::new(), String::new());
    }
    (format!("{host}:{port}"), fingerprint)
}

/// Result of a successful registration.
pub struct Registration {
    pub node_id: String,
    pub organization_id: String,
    pub platform_url: String,
}

/// Write a secret file with 0600 permissions, creating its directory.
fn write_secret(path: &Path, value: &str) -> Result<()> {
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("cannot create directory {}", dir.display()))?;
    }
    std::fs::write(path, value).with_context(|| format!("cannot write {}", path.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("cannot set permissions on {}", path.display()))?;
    }
    Ok(())
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|value| value.trim().to_string())
        .unwrap_or_default()
}

/// Best-effort primary address: whichever source address the kernel picks for
/// a public destination. No packet is sent — this only asks the routing table.
fn primary_ip() -> Option<String> {
    let out = std::process::Command::new("ip")
        .args(["-o", "route", "get", "1.1.1.1"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut fields = text.split_whitespace();
    while let Some(field) = fields.next() {
        if field == "src" {
            return fields.next().map(str::to_string);
        }
    }
    None
}

fn os_description() -> String {
    let Ok(release) = std::fs::read_to_string("/etc/os-release") else {
        return "linux".to_string();
    };
    for line in release.lines() {
        if let Some(value) = line.strip_prefix("PRETTY_NAME=") {
            return value.trim_matches('"').to_string();
        }
    }
    "linux".to_string()
}

/// RFC 3339 timestamp without pulling in a date library: the daemon only needs
/// to record when registration happened.
fn now_rfc3339() -> String {
    let out = std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output();
    match out {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => {
            warn!("cannot determine the current time; registration timestamp omitted");
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_restricted_to_url_safe_characters() {
        assert!(valid_token("abcDEF-123_456"));
        assert!(!valid_token(""));
        assert!(!valid_token("has space"));
        assert!(!valid_token("semi;colon"));
        assert!(!valid_token(&"x".repeat(129)));
    }

    #[test]
    fn urls_must_be_http_and_lose_their_trailing_slash() {
        assert_eq!(
            normalize_url("https://adminservice.cloud/").unwrap(),
            "https://adminservice.cloud"
        );
        assert_eq!(
            normalize_url("http://localhost:3000").unwrap(),
            "http://localhost:3000"
        );
        assert!(normalize_url("ftp://example.com").is_err());
        assert!(normalize_url("adminservice.cloud").is_err());
    }
}
