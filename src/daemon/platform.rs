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
//!
//! Registration also gives the platform its way in (DMN-145): the daemon
//! reports how sshd is reached and which host keys it presents, and installs
//! the public key the platform answers with into root's `authorized_keys`.
//! Without that a node added with the install command was registered and
//! still unreachable — nothing else ever puts a platform key on it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tracing::warn;

use crate::daemon::config::Config;
use crate::daemon::http;

/// The account the platform logs in as. Registration runs as root, and root
/// is what an SSH-provisioned node is reached as too.
const SSH_USER: &str = "root";

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
    /// The platform's key for this node; empty when it already had SSH
    /// access or did not ask for any.
    #[serde(default, rename = "sshAuthorizedKey")]
    ssh_authorized_key: String,
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
    let advertised = direct_endpoint(config);
    let api_endpoint = advertised.endpoint.clone();
    // Handed over in both modes: a node that registers itself was never
    // logged into by the platform, so nothing else would ever read it — over
    // SSH included (the platform used to read it only after provisioning).
    let api_token = std::fs::read_to_string(crate::daemon::api::api_token_path())
        .map(|value| value.trim().to_string())
        .unwrap_or_default();
    let body = serde_json::json!({
        "token": token,
        "hostname": hostname(),
        "primaryIp": primary_ip().unwrap_or_default(),
        "os": os_description(),
        "arch": std::env::consts::ARCH,
        "daemonVersion": crate::VERSION,
        "apiEndpoint": api_endpoint,
        "tlsFingerprint": advertised.fingerprint,
        "tlsMode": advertised.tls_mode,
        "domain": advertised.domain,
        "apiToken": api_token,
        "sshPort": ssh_port(),
        "sshUser": SSH_USER,
        "sshHostKeys": ssh_host_keys(),
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

    // The node is registered either way: a key that cannot be installed is
    // reported, and the operator can still add it by hand or finish the
    // connection settings on the platform.
    let ssh_access = install_platform_key(&parsed.ssh_authorized_key);

    Ok(Registration {
        node_id: parsed.node_id,
        organization_id: parsed.organization_id,
        platform_url,
        ssh_access,
    })
}

/// What became of the platform's SSH key.
#[derive(Debug, PartialEq, Eq)]
pub enum SshAccess {
    /// The key is in root's `authorized_keys` (or already was).
    Granted,
    /// The platform sent none: the node already had SSH credentials there.
    NotOffered,
    Failed(String),
}

/// Tell the operator running the install what became of the key — shared by
/// `asc connect` and `asc-updater install --token`.
pub fn print_ssh_access(access: &SshAccess) {
    use crate::daemon::i18n::{Msg, t, tf};
    match access {
        SshAccess::Granted => println!("{}", t(Msg::PlatformSshGranted)),
        SshAccess::NotOffered => {}
        SshAccess::Failed(reason) => eprintln!("{}", tf(Msg::PlatformSshFailed, reason)),
    }
}

fn install_platform_key(key: &str) -> SshAccess {
    let key = key.trim();
    if key.is_empty() {
        return SshAccess::NotOffered;
    }
    match crate::daemon::users::add_authorized_key(SSH_USER, key) {
        Ok(_) => SshAccess::Granted,
        Err(err) => {
            warn!("cannot install the platform ssh key: {err}");
            SshAccess::Failed(err.to_string())
        }
    }
}

/// sshd's effective port. `sshd -T` answers with the configuration it would
/// actually run with (includes, Match-free defaults); reading sshd_config is
/// the fallback for when it cannot — no host keys yet, or no sshd binary on
/// PATH — and 22 the last resort.
fn ssh_port() -> u16 {
    for binary in ["sshd", "/usr/sbin/sshd"] {
        if let Ok(out) = std::process::Command::new(binary).arg("-T").output()
            && out.status.success()
            && let Some(port) = port_from_sshd_t(&String::from_utf8_lossy(&out.stdout))
        {
            return port;
        }
    }
    std::fs::read_to_string("/etc/ssh/sshd_config")
        .ok()
        .and_then(|text| port_from_sshd_config(&text))
        .unwrap_or(22)
}

/// The first `port N` line of `sshd -T` output (lowercase keys, one per line).
fn port_from_sshd_t(text: &str) -> Option<u16> {
    text.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        (parts.next() == Some("port"))
            .then(|| parts.next()?.parse().ok())
            .flatten()
    })
}

/// The first global `Port` directive of sshd_config. Keywords are
/// case-insensitive; everything after a `Match` block starts applies only
/// conditionally, so the scan stops there.
fn port_from_sshd_config(text: &str) -> Option<u16> {
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let mut parts = line
            .split(|c: char| c.is_whitespace() || c == '=')
            .filter(|p| !p.is_empty());
        let Some(keyword) = parts.next() else {
            continue;
        };
        if keyword.eq_ignore_ascii_case("match") {
            return None;
        }
        if keyword.eq_ignore_ascii_case("port") {
            return parts.next()?.parse().ok();
        }
    }
    None
}

/// The host's public keys as `<algo> <base64>`, for the platform to pin the
/// one its SSH client will be shown. Comments are dropped: they name the
/// machine at the time the key was generated, nothing the pin needs.
fn ssh_host_keys() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/etc/ssh") else {
        return Vec::new();
    };
    let mut names: Vec<_> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("ssh_host_") && name.ends_with("_key.pub"))
        .collect();
    names.sort();
    names
        .iter()
        .filter_map(|name| std::fs::read_to_string(Path::new("/etc/ssh").join(name)).ok())
        .filter_map(|text| host_key_line(&text))
        .collect()
}

fn host_key_line(text: &str) -> Option<String> {
    let mut parts = text.split_whitespace();
    let algo = parts.next()?;
    let blob = parts.next()?;
    (algo.starts_with("ssh-") || algo.starts_with("ecdsa-")).then(|| format!("{algo} {blob}"))
}

/// What the platform needs in order to dial this daemon directly.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Advertised {
    /// `host:port`, empty when direct access is not available.
    pub endpoint: String,
    /// Set only for a self-signed certificate: that is the one case where no
    /// CA vouches for it and the platform has to pin it instead.
    pub fingerprint: String,
    /// How the certificate is trusted: self_signed | acme | files.
    pub tls_mode: String,
    pub domain: String,
}

/// Where the platform can dial this daemon directly, if anywhere. An API bound
/// to loopback is not reachable from outside, and one served without TLS must
/// not be advertised: the bearer token would cross the network in the clear.
pub fn direct_endpoint(config: &Config) -> Advertised {
    use crate::daemon::api::tls;
    use crate::daemon::config::TlsMode;

    if config.api.tls == TlsMode::Off {
        return Advertised::default();
    }
    let listen = config.api.listen.clone();
    if listen.starts_with("127.") || listen.starts_with("localhost") {
        return Advertised::default();
    }
    let port = listen.rsplit(':').next().unwrap_or("8420").to_string();
    // A domain outlives an address, so it wins whenever one is configured.
    let domain = config.api.domain.clone().unwrap_or_default();
    let host = if domain.is_empty() {
        primary_ip().unwrap_or_default()
    } else {
        domain.clone()
    };
    if host.is_empty() {
        return Advertised::default();
    }

    // Self-signed must be pinned. An operator's own certificate is reported
    // too (DMN-127): it may come from a private CA — a Cloudflare Origin
    // certificate is one — where the fingerprint is the only thing that can
    // vouch for it; the platform still verifies the chain when it can. ACME
    // is chain-verified only: pinning it would break at the first renewal.
    let fingerprint = match config.api.tls {
        TlsMode::SelfSigned => {
            let print = tls::current_fingerprint().unwrap_or_default();
            if print.is_empty() {
                return Advertised::default();
            }
            print
        }
        TlsMode::Files => config
            .api
            .tls_cert
            .as_deref()
            .and_then(tls::fingerprint_of)
            .unwrap_or_default(),
        _ => String::new(),
    };
    Advertised {
        endpoint: format!("{host}:{port}"),
        fingerprint,
        tls_mode: config.api.tls.as_str().to_string(),
        domain,
    }
}

/// Tell the platform the address or certificate changed after registration
/// (DMN-068).
///
/// Registration is a one-shot token redemption, so nothing else can carry the
/// news that a certificate was renewed or an address moved. The call
/// authenticates with the node's primary API token, which the platform already
/// holds — the only secret the two sides share without a user session.
///
/// Best-effort by design: a node that cannot reach the platform right now is
/// still a working node, and the health poller reconciles the drift anyway.
pub fn report_endpoint(config: &mut Config) -> Result<bool> {
    let (Some(platform_url), Some(node_id)) =
        (config.platform.url.clone(), config.platform.node_id.clone())
    else {
        return Ok(false);
    };
    let advertised = direct_endpoint(config);
    // Nothing changed since the last report: the platform already knows.
    if config.platform.reported_endpoint.as_deref() == Some(advertised.endpoint.as_str())
        && config.platform.reported_fingerprint.as_deref() == Some(advertised.fingerprint.as_str())
    {
        return Ok(false);
    }

    let api_token = std::fs::read_to_string(crate::daemon::api::api_token_path())
        .map(|value| value.trim().to_string())
        .unwrap_or_default();
    if api_token.is_empty() {
        bail!("no API token to authenticate the report with");
    }

    let body = serde_json::json!({
        "nodeId": node_id,
        "apiToken": api_token,
        "apiEndpoint": advertised.endpoint,
        "tlsFingerprint": advertised.fingerprint,
        "tlsMode": advertised.tls_mode,
        "domain": advertised.domain,
        "daemonVersion": crate::VERSION,
    })
    .to_string();
    let endpoint =
        format!("{platform_url}/bootstrap/asc.node.v1.BootstrapService/ReportNodeEndpoint");
    http::post_json(&endpoint, &body)?;

    config.platform.reported_endpoint = Some(advertised.endpoint);
    config.platform.reported_fingerprint = Some(advertised.fingerprint);
    config.save().context("cannot save config.toml")?;
    Ok(true)
}

/// Result of a successful registration.
pub struct Registration {
    pub node_id: String,
    pub organization_id: String,
    pub platform_url: String,
    pub ssh_access: SshAccess,
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

    #[test]
    fn sshd_port_is_read_from_the_effective_configuration() {
        assert_eq!(
            port_from_sshd_t("permitrootlogin yes\nport 2222\nport 22\n"),
            Some(2222)
        );
        assert_eq!(port_from_sshd_t("passwordauthentication no\n"), None);
    }

    #[test]
    fn sshd_config_port_stops_at_the_first_match_block() {
        assert_eq!(
            port_from_sshd_config("# Port 1\n  port = 2200\n"),
            Some(2200)
        );
        assert_eq!(port_from_sshd_config("Port\t2022\nPort 22\n"), Some(2022));
        assert_eq!(port_from_sshd_config("Match User git\n  Port 2222\n"), None);
        assert_eq!(port_from_sshd_config("PermitRootLogin yes\n"), None);
    }

    #[test]
    fn host_keys_lose_their_comment() {
        assert_eq!(
            host_key_line("ssh-ed25519 AAAAC3Nza root@node\n").as_deref(),
            Some("ssh-ed25519 AAAAC3Nza")
        );
        assert_eq!(
            host_key_line("ecdsa-sha2-nistp256 AAAAE2Vj").as_deref(),
            Some("ecdsa-sha2-nistp256 AAAAE2Vj")
        );
        assert_eq!(host_key_line("garbage"), None);
        assert_eq!(host_key_line("command=\"x\" ssh-rsa AAAA"), None);
    }

    #[test]
    fn no_key_from_the_platform_means_nothing_to_install() {
        assert_eq!(install_platform_key("  "), SshAccess::NotOffered);
    }
}
