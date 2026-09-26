//! Data model of the web server module (DMN-122/DMN-123): node-wide
//! settings, sites and their statuses, with the validation every write goes
//! through. Everything here is plain data — rendering lives in
//! [`super::render`], side effects in [`super::apply`] and [`super::engine`].

use std::collections::BTreeSet;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// Where nginx comes from and how it is driven.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Not enabled on this node.
    #[default]
    None,
    /// Distribution / nginx.org package under systemd.
    System,
    /// `asc-webserver` container on the host network.
    Docker,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::System => "system",
            Self::Docker => "docker",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "system" => Ok(Self::System),
            "docker" => Ok(Self::Docker),
            other => bail!("unknown web server mode {other:?}: expected system or docker"),
        }
    }
}

/// Facts about the host nginx taken over or installed, captured once at
/// install time so the generated `nginx.conf` keeps what the distribution's
/// systemd unit and packaging expect.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HostProfile {
    /// `user` directive (`nginx` on nginx.org packages, `www-data` on Debian).
    pub user: String,
    /// `pid` directive — must match the systemd unit's `PIDFile`.
    pub pid: String,
    /// Debian's dynamic modules (`/etc/nginx/modules-enabled/*.conf`).
    pub include_modules: bool,
    /// The operator's own `conf.d/*.conf` and `sites-enabled/*`.
    pub include_conf_d: bool,
    pub include_sites_enabled: bool,
    /// Another config already declares `default_server` on :80 / :443 — ours
    /// is then left out, two would make `nginx -t` fail.
    pub foreign_default_http: bool,
    pub foreign_default_https: bool,
    /// The daemon adopted an nginx it did not install: uninstall restores
    /// the original `nginx.conf` instead of removing the package.
    pub adopted: bool,
    /// The package came from the nginx.org repository the daemon added.
    pub nginx_org_repo: bool,
}

/// Node-wide web server settings — `/etc/asc/webserver/webserver.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub mode: Mode,
    pub image: String,
    pub worker_processes: String,
    pub worker_connections: u32,
    pub keepalive_timeout: u32,
    pub client_max_body_size: String,
    pub gzip: bool,
    pub gzip_level: u32,
    pub server_tokens: bool,
    pub http2: bool,
    pub tls_protocols: Vec<String>,
    pub hsts: bool,
    pub cloudflare_real_ip: bool,
    pub access_log: bool,
    pub acme_email: String,
    pub acme_directory: String,
    pub custom_main: String,
    pub custom_http: String,
    pub host: HostProfile,
}

pub const DEFAULT_IMAGE: &str = "nginx:stable-alpine";

impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: Mode::None,
            image: DEFAULT_IMAGE.to_string(),
            worker_processes: "auto".to_string(),
            worker_connections: 4096,
            keepalive_timeout: 65,
            client_max_body_size: "64m".to_string(),
            gzip: true,
            gzip_level: 5,
            server_tokens: false,
            http2: true,
            tls_protocols: vec!["TLSv1.2".to_string(), "TLSv1.3".to_string()],
            hsts: false,
            cloudflare_real_ip: false,
            access_log: true,
            acme_email: String::new(),
            acme_directory: String::new(),
            custom_main: String::new(),
            custom_http: String::new(),
            host: HostProfile::default(),
        }
    }
}

impl Settings {
    /// The image the docker mode runs.
    pub fn image(&self) -> &str {
        if self.image.trim().is_empty() {
            DEFAULT_IMAGE
        } else {
            self.image.trim()
        }
    }

    /// Checks everything the renderer pastes into directives. Free-form
    /// snippets (`custom_*`) are left to `nginx -t`.
    pub fn validate(&self) -> Result<()> {
        let wp = self.worker_processes.trim();
        if wp != "auto" && !(wp.parse::<u32>().is_ok_and(|n| (1..=1024).contains(&n))) {
            bail!("worker_processes must be \"auto\" or 1..1024, got {wp:?}");
        }
        if !(64..=1_048_576).contains(&self.worker_connections) {
            bail!("worker_connections must be within 64..1048576");
        }
        if self.keepalive_timeout > 3600 {
            bail!("keepalive_timeout must be at most 3600 seconds");
        }
        validate_size("client_max_body_size", &self.client_max_body_size)?;
        if !(1..=9).contains(&self.gzip_level) {
            bail!("gzip_level must be within 1..9");
        }
        if self.tls_protocols.is_empty() {
            bail!("at least one TLS protocol is required");
        }
        for protocol in &self.tls_protocols {
            if !matches!(
                protocol.as_str(),
                "TLSv1" | "TLSv1.1" | "TLSv1.2" | "TLSv1.3"
            ) {
                bail!("unknown TLS protocol {protocol:?}");
            }
        }
        if !self.acme_email.is_empty() && !plausible_email(&self.acme_email) {
            bail!("acme_email {:?} is not an e-mail address", self.acme_email);
        }
        if !self.acme_directory.is_empty() && !self.acme_directory.starts_with("https://") {
            bail!("acme_directory must be an https:// URL");
        }
        if self
            .image
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
        {
            bail!("image must not contain whitespace");
        }
        Ok(())
    }
}

/// nginx size syntax: digits with an optional k/m/g suffix.
pub fn validate_size(field: &str, value: &str) -> Result<()> {
    let value = value.trim();
    let digits = value.trim_end_matches(['k', 'K', 'm', 'M', 'g', 'G']);
    if value.is_empty()
        || digits.is_empty()
        || value.len() - digits.len() > 1
        || !digits.chars().all(|c| c.is_ascii_digit())
        || digits.len() > 12
    {
        bail!("{field} must be a size like 64m or 1g, got {value:?}");
    }
    Ok(())
}

fn plausible_email(value: &str) -> bool {
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && value
            .chars()
            .all(|c| c.is_ascii_graphic() && !matches!(c, '"' | ';' | '\\' | '<' | '>'))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Balance {
    #[default]
    RoundRobin,
    LeastConn,
    IpHash,
    Hash,
}

/// What an upstream server points at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Target {
    /// An installed app: id, custom name or uuid, plus its container-side
    /// port — the daemon resolves the published host port itself.
    App { app: String, port: u16 },
    /// Any `host:port`.
    Address { address: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpstreamServer {
    #[serde(flatten)]
    pub target: Target,
    #[serde(default = "one")]
    pub weight: u32,
    #[serde(default)]
    pub backup: bool,
    #[serde(default)]
    pub max_fails: u32,
    #[serde(default)]
    pub fail_timeout_secs: u32,
    #[serde(default)]
    pub down: bool,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthKind {
    #[default]
    Off,
    Tcp,
    Http,
}

/// Active health checks of a site's upstream servers (DMN-126). Zero values
/// mean the defaults below.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HealthCheck {
    pub kind: HealthKind,
    /// HTTP: request path.
    pub path: String,
    /// HTTP: expected status; 0 — any 2xx/3xx.
    pub expected_status: u16,
    pub interval_secs: u32,
    pub timeout_secs: u32,
    /// Failures in a row that take a server out of rotation.
    pub fails: u32,
    /// Passes in a row that bring it back.
    pub passes: u32,
}

impl HealthCheck {
    pub fn enabled(&self) -> bool {
        self.kind != HealthKind::Off
    }
    pub fn interval(&self) -> u32 {
        if self.interval_secs == 0 {
            5
        } else {
            self.interval_secs
        }
    }
    pub fn timeout(&self) -> u32 {
        if self.timeout_secs == 0 {
            2
        } else {
            self.timeout_secs
        }
    }
    pub fn fails(&self) -> u32 {
        if self.fails == 0 { 3 } else { self.fails }
    }
    pub fn passes(&self) -> u32 {
        if self.passes == 0 { 2 } else { self.passes }
    }

    fn validate(&mut self) -> Result<()> {
        if !self.enabled() {
            *self = HealthCheck::default();
            return Ok(());
        }
        if self.interval_secs > 3600
            || self.timeout_secs > 60
            || self.fails > 20
            || self.passes > 20
        {
            bail!("health check: interval ≤ 3600 s, timeout ≤ 60 s, fails and passes ≤ 20");
        }
        if self.kind == HealthKind::Http {
            if self.path.is_empty() {
                self.path = "/".into();
            }
            if !self.path.starts_with('/')
                || self.path.len() > 1024
                || self
                    .path
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control())
            {
                bail!("health check path must start with / and contain no spaces");
            }
            if self.expected_status != 0 && !(100..=599).contains(&self.expected_status) {
                bail!("health check expected status must be 100..599");
            }
        } else {
            self.path.clear();
            self.expected_status = 0;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Upstream {
    pub servers: Vec<UpstreamServer>,
    pub balance: Balance,
    pub hash_key: String,
    pub keepalive: u32,
    /// Talk HTTPS to the upstream servers.
    pub tls: bool,
    pub health_check: HealthCheck,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    #[default]
    None,
    /// Let's Encrypt (or `acme_directory`), issued and renewed by the daemon.
    Acme,
    /// Certificate and key handed in by the caller.
    Provided,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tls {
    pub mode: TlsMode,
    /// `Provided` only. Stored in `sites.json` (0600) and written to the
    /// certificate directory; never returned through the API.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub certificate_pem: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub private_key_pem: String,
    pub redirect_http: bool,
    pub hsts: bool,
    pub http2: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Proxy {
    pub websocket: bool,
    pub client_max_body_size: String,
    pub connect_timeout_secs: u32,
    pub read_timeout_secs: u32,
    pub send_timeout_secs: u32,
    pub request_headers: Vec<Header>,
    pub response_headers: Vec<Header>,
    /// Send the upstream its own address as `Host` instead of the client's.
    pub upstream_host: bool,
    pub no_buffering: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RealIp {
    #[default]
    Off,
    Cloudflare,
}

/// One virtual host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Site {
    pub id: String,
    pub server_names: Vec<String>,
    /// `None` — a local site (`asc web site add`); the platform pushes its
    /// own as `"platform"` and only ever replaces those.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_by: Option<String>,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub upstream: Upstream,
    #[serde(default)]
    pub tls: Tls,
    #[serde(default)]
    pub proxy: Proxy,
    #[serde(default)]
    pub real_ip: RealIp,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub extra_server: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub extra_location: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_config: Option<String>,
}

pub const MAX_SITE_ID: usize = 64;
const MAX_SNIPPET: usize = 64 * 1024;
const MAX_NAMES: usize = 100;

pub fn validate_site_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= MAX_SITE_ID
        && id
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && id.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
        })
        && !id.contains("..");
    if !ok {
        bail!("site id {id:?} must be 1-64 characters of a-z, 0-9, '-', '_' or '.'");
    }
    Ok(())
}

/// A DNS name nginx will accept verbatim in `server_name`: a hostname, or a
/// leading-wildcard `*.example.com`. Regex server names are deliberately not
/// supported — they cannot be issued certificates and would be a way to
/// smuggle directives in.
pub fn validate_server_name(name: &str) -> Result<()> {
    let host = name.strip_prefix("*.").unwrap_or(name);
    let ok = !host.is_empty()
        && name.len() <= 253
        && host.split('.').count() >= 2
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        });
    if !ok {
        bail!("{name:?} is not a valid lowercase domain name");
    }
    Ok(())
}

/// `host:port` with a hostname, IPv4 or bracketed IPv6 host.
pub fn validate_address(address: &str) -> Result<()> {
    let bad = || anyhow::anyhow!("{address:?} is not a host:port address");
    let (host, port) = address.rsplit_once(':').ok_or_else(bad)?;
    port.parse::<u16>()
        .ok()
        .filter(|p| *p != 0)
        .ok_or_else(bad)?;
    let host_ok = if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        inner.parse::<std::net::Ipv6Addr>().is_ok()
    } else {
        !host.is_empty()
            && host.len() <= 253
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
    };
    if !host_ok {
        return Err(bad());
    }
    Ok(())
}

fn validate_header(header: &Header) -> Result<()> {
    if header.name.is_empty()
        || !header
            .name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        bail!("header name {:?} is invalid", header.name);
    }
    // The value is emitted inside double quotes: forbid what would end the
    // string or the directive.
    if header
        .value
        .chars()
        .any(|c| c.is_control() || matches!(c, '"' | '\\' | ';' | '{' | '}'))
    {
        bail!(
            "header {:?} value contains a forbidden character",
            header.name
        );
    }
    Ok(())
}

fn validate_snippet(field: &str, text: &str) -> Result<()> {
    if text.len() > MAX_SNIPPET {
        bail!("{field} is larger than {} KiB", MAX_SNIPPET / 1024);
    }
    if text.contains('\0') {
        bail!("{field} contains a NUL byte");
    }
    Ok(())
}

impl Site {
    pub fn is_local(&self) -> bool {
        self.managed_by.is_none()
    }

    /// Normalizes (lowercase names, sorted & deduplicated) and validates.
    pub fn normalize(&mut self) -> Result<()> {
        validate_site_id(&self.id)?;
        let names: BTreeSet<String> = self
            .server_names
            .iter()
            .map(|n| n.trim().trim_end_matches('.').to_ascii_lowercase())
            .filter(|n| !n.is_empty())
            .collect();
        if names.is_empty() {
            bail!("site {:?} needs at least one server name", self.id);
        }
        if names.len() > MAX_NAMES {
            bail!("site {:?} has more than {MAX_NAMES} server names", self.id);
        }
        for name in &names {
            validate_server_name(name)?;
        }
        self.server_names = names.into_iter().collect();
        if let Some(marker) = &self.managed_by
            && (marker.is_empty() || marker.len() > 64 || marker.chars().any(char::is_whitespace))
        {
            bail!("managed_by {marker:?} is invalid");
        }

        if self.raw_config.is_none() && self.upstream.servers.is_empty() {
            bail!("site {:?} has no upstream servers", self.id);
        }
        for server in &self.upstream.servers {
            match &server.target {
                Target::App { app, port } => {
                    if app.trim().is_empty() || *port == 0 {
                        bail!("an app upstream needs an app and a port");
                    }
                }
                Target::Address { address } => validate_address(address)?,
            }
            if server.weight == 0 || server.weight > 1000 {
                bail!("upstream weight must be within 1..1000");
            }
        }
        if self.upstream.balance == Balance::Hash {
            let key = self.upstream.hash_key.trim();
            if key.is_empty()
                || key
                    .chars()
                    .any(|c| c.is_whitespace() || matches!(c, ';' | '{' | '}' | '"' | '\''))
            {
                bail!("hash balancing needs a hash key without spaces, e.g. $cookie_session");
            }
        }
        if self.upstream.keepalive > 1024 {
            bail!("upstream keepalive must be at most 1024");
        }
        self.upstream.health_check.validate()?;

        match self.tls.mode {
            TlsMode::Provided => {
                if self.tls.certificate_pem.trim().is_empty()
                    || self.tls.private_key_pem.trim().is_empty()
                {
                    bail!("a provided certificate needs both the certificate and the key");
                }
                super::cert::check_pair(&self.tls.certificate_pem, &self.tls.private_key_pem)?;
            }
            TlsMode::Acme => {
                if self.server_names.iter().any(|n| n.starts_with("*.")) {
                    bail!("wildcard names cannot be issued over HTTP-01; provide a certificate");
                }
                self.tls.certificate_pem.clear();
                self.tls.private_key_pem.clear();
            }
            TlsMode::None => {
                self.tls.certificate_pem.clear();
                self.tls.private_key_pem.clear();
            }
        }

        if !self.proxy.client_max_body_size.is_empty() {
            validate_size("client_max_body_size", &self.proxy.client_max_body_size)?;
        }
        for timeout in [
            self.proxy.connect_timeout_secs,
            self.proxy.read_timeout_secs,
            self.proxy.send_timeout_secs,
        ] {
            if timeout > 86_400 {
                bail!("proxy timeouts must be at most 86400 seconds");
            }
        }
        for header in self
            .proxy
            .request_headers
            .iter()
            .chain(&self.proxy.response_headers)
        {
            validate_header(header)?;
        }
        validate_snippet("extra_server", &self.extra_server)?;
        validate_snippet("extra_location", &self.extra_location)?;
        if let Some(raw) = &self.raw_config {
            if raw.trim().is_empty() {
                bail!("raw_config is empty");
            }
            validate_snippet("raw_config", raw)?;
        }
        Ok(())
    }

    /// The site as the API shows it: no private material.
    pub fn redacted(&self) -> Self {
        let mut site = self.clone();
        site.tls.certificate_pem.clear();
        site.tls.private_key_pem.clear();
        site
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SiteState {
    #[default]
    Pending,
    Applied,
    Error,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsState {
    #[default]
    None,
    PendingDns,
    Issuing,
    Active,
    Expiring,
    Error,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TlsStatus {
    pub state: TlsState,
    pub not_after: i64,
    pub issuer: String,
    pub last_error: String,
    pub next_attempt: i64,
    /// ACME failures in a row — drives the backoff.
    pub failures: u32,
    /// Names the current certificate was issued for; a change re-issues.
    pub names: Vec<String>,
}

/// One upstream server as the health checks see it (DMN-126).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpstreamHealth {
    pub address: String,
    pub healthy: bool,
    pub checked: bool,
    pub last_error: String,
    pub checked_at: i64,
    pub latency_ms: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SiteStatus {
    pub state: SiteState,
    pub message: String,
    pub applied_at: i64,
    pub tls: TlsStatus,
    pub upstream_addresses: Vec<String>,
    /// Filled from the live health registry when a view is built; never
    /// persisted meaningfully.
    pub upstream_health: Vec<UpstreamHealth>,
}

/// A site together with its status, as the API returns it.
#[derive(Debug, Clone, Serialize)]
pub struct SiteView {
    #[serde(flatten)]
    pub site: Site,
    pub status: SiteStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(names: &[&str]) -> Site {
        Site {
            id: "app-1".into(),
            server_names: names.iter().map(|n| n.to_string()).collect(),
            managed_by: None,
            disabled: false,
            upstream: Upstream {
                servers: vec![UpstreamServer {
                    target: Target::Address {
                        address: "127.0.0.1:8080".into(),
                    },
                    weight: 1,
                    backup: false,
                    max_fails: 0,
                    fail_timeout_secs: 0,
                    down: false,
                }],
                ..Default::default()
            },
            tls: Tls::default(),
            proxy: Proxy::default(),
            real_ip: RealIp::Off,
            extra_server: String::new(),
            extra_location: String::new(),
            raw_config: None,
        }
    }

    #[test]
    fn names_are_normalized_and_deduplicated() {
        let mut s = site(&["B.Example.com.", "b.example.com", "a.example.com"]);
        s.normalize().unwrap();
        assert_eq!(s.server_names, vec!["a.example.com", "b.example.com"]);
    }

    #[test]
    fn server_names_reject_injection() {
        for bad in [
            "example",
            "exa mple.com",
            "a.com;include /etc/passwd",
            "~^(.+)$",
            "-a.com",
            "*.*.com",
            "",
        ] {
            assert!(validate_server_name(bad).is_err(), "{bad}");
        }
        validate_server_name("*.example.com").unwrap();
        validate_server_name("xn--80ak6aa92e.com").unwrap();
    }

    #[test]
    fn addresses() {
        validate_address("127.0.0.1:80").unwrap();
        validate_address("[::1]:8080").unwrap();
        validate_address("backend.internal:3000").unwrap();
        for bad in [
            "127.0.0.1",
            "host:0",
            "host:99999",
            "a b:80",
            "h;x:80",
            "[zz]:80",
        ] {
            assert!(validate_address(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn site_ids() {
        validate_site_id("0f7c1d9e-2a4b-4c1d-9e2a-4b4c1d9e2a4b").unwrap();
        for bad in ["", "A", "-a", "a/b", "..", "a..b", &"a".repeat(65)] {
            assert!(validate_site_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn headers_cannot_break_out_of_quotes() {
        let mut s = site(&["a.example.com"]);
        s.proxy.request_headers.push(Header {
            name: "X-Test".into(),
            value: "a\"; include /etc/shadow; #".into(),
        });
        assert!(s.normalize().is_err());
    }

    #[test]
    fn acme_refuses_wildcards() {
        let mut s = site(&["*.example.com"]);
        s.tls.mode = TlsMode::Acme;
        assert!(s.normalize().is_err());
    }

    #[test]
    fn sizes() {
        for good in ["0", "64m", "1G", "512k", "1024"] {
            validate_size("x", good).unwrap();
        }
        for bad in ["", "m", "1mm", "1 m", "-1m", "1t"] {
            assert!(validate_size("x", bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn settings_round_trip_through_toml() {
        let settings = Settings {
            mode: Mode::Docker,
            ..Default::default()
        };
        let text = toml::to_string_pretty(&settings).unwrap();
        let back: Settings = toml::from_str(&format!("# comment — dash\n{text}")).unwrap();
        assert_eq!(back, settings);
    }

    #[test]
    fn settings_defaults_validate() {
        Settings::default().validate().unwrap();
        let s = Settings {
            worker_processes: "many".into(),
            ..Default::default()
        };
        assert!(s.validate().is_err());
    }

    #[test]
    fn redaction_drops_keys() {
        let mut s = site(&["a.example.com"]);
        s.tls.certificate_pem = "cert".into();
        s.tls.private_key_pem = "key".into();
        let r = s.redacted();
        assert!(r.tls.certificate_pem.is_empty() && r.tls.private_key_pem.is_empty());
    }
}
