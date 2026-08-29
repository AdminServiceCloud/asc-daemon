//! Daemon configuration: `/etc/asc/config.toml` for root, `~/.asc/config.toml`
//! for regular users.
//!
//! Missing file means defaults — the daemon must run without any setup.
//! The path can be overridden with the `ASC_CONFIG` environment variable
//! (used by tests and local development).
//!
//! Root (the system daemon, sudo) uses the shared system paths
//! (`/etc/asc`, `/var/lib/asc`, `/asc/apps`, `/usr/local/bin`). A regular
//! user gets a private tree under `~/.asc/` instead — config, data, apps and
//! binaries they can edit without sudo (see [`user_base_dir`]).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::daemon::i18n::Lang;

const DEFAULT_CONFIG_PATH: &str = "/etc/asc/config.toml";
const DEFAULT_DATA_DIR: &str = "/var/lib/asc";
const DEFAULT_APPS_DIR: &str = "/asc/apps";

/// Whether the process runs as root (euid 0) — root manages the shared
/// system installation, everyone else a private one under `~/.asc`.
pub fn is_root() -> bool {
    // SAFETY: geteuid() has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// The per-user base directory `~/.asc` — config, data, apps and binaries of
/// a non-root installation, all editable without sudo. `None` when `$HOME`
/// is unset (then the system paths are used as a last resort).
pub fn user_base_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".asc"))
}

/// `~/.asc/<sub>` for regular users, `<system>` for root (or without $HOME).
fn scoped_path(sub: &str, system: &str) -> PathBuf {
    match user_base_dir() {
        Some(base) if !is_root() => base.join(sub),
        _ => PathBuf::from(system),
    }
}

/// Root of `config.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// CLI output language (`en` / `ru`), see `asc config lang`.
    pub language: Lang,
    pub log: LogConfig,
    pub daemon: DaemonConfig,
    pub docker: DockerConfig,
    pub api: ApiConfig,
    pub monitor: MonitorConfig,
    pub policy: PolicyConfig,
    pub updater: UpdaterConfig,
    pub platform: PlatformConfig,
}

/// `[platform]` — binding to AdminService.Cloud (DMN-058).
///
/// Non-secret values only: the registration token lives in `platform.token`
/// next to this file, root-readable at 0600, because config.toml is
/// world-readable by design.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PlatformConfig {
    /// Base URL of the platform this node reports to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Node id assigned by the platform on registration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// When registration succeeded, RFC 3339.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registered_at: Option<String>,
    /// What was last reported to the platform (DMN-068). Not a secret and not
    /// a setting — a cache, so a daemon that restarts with nothing changed
    /// does not call the platform on every boot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reported_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reported_fingerprint: Option<String>,
}

/// `[policy]` — root-managed rules for regular (non-root) users.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicyConfig {
    /// What regular users may install: everything or Docker apps only.
    /// Root is never restricted.
    pub user_install: UserInstall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserInstall {
    /// Docker, native apps and utilities.
    #[default]
    All,
    /// Docker apps only; native apps and utilities need root.
    Docker,
}

/// `[monitor]` — system metrics sampling (DMN-006).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MonitorConfig {
    /// Seconds between samples.
    pub interval_secs: u64,
    /// Ring buffer depth (360 × 10 s = one hour of history in memory).
    pub history_samples: usize,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            interval_secs: 10,
            history_samples: 360,
        }
    }
}

/// `[docker]` — connection to the Docker Engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DockerConfig {
    /// Path to the Docker daemon unix socket. The daemon talks to Docker
    /// through the Engine API over this socket (not the `docker` CLI), so
    /// non-standard installs (rootless, custom `DOCKER_HOST`) just point here.
    pub socket: PathBuf,
}

impl Default for DockerConfig {
    fn default() -> Self {
        Self {
            socket: PathBuf::from("/var/run/docker.sock"),
        }
    }
}

/// `[api]` — the daemon API server (gRPC + REST on one listener).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    /// Listen address. Localhost by default: remote access goes through the
    /// platform tunnel, not an exposed port.
    pub listen: String,
    /// Local unix socket of the same API, authenticated by SO_PEERCRED
    /// instead of the bearer token: the CLI talks to the daemon here, and
    /// the daemon enforces per-user app ownership from the kernel-reported
    /// peer uid. The default is the system daemon's socket for everyone —
    /// a private non-root daemon (DMN-041) overrides it in its own config.
    pub socket: PathBuf,
    /// Legacy field: the token now lives in `api.token` next to config.toml
    /// (root-only 0600, see `api::api_token_path`). Kept for migration —
    /// a value found here is moved out on the next daemon start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// TLS for the API listener (DMN-061). Off is correct while the port is
    /// on loopback; a node reached directly must encrypt, because the bearer
    /// token grants full control of the machine.
    pub tls: TlsMode,
    /// Certificate and key for `tls = "files"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_cert: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_key: Option<PathBuf>,
    /// Extra names baked into the self-signed certificate — typically the
    /// node's public IP, so the platform can dial it by address.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tls_sans: Vec<String>,
    /// The DNS name this node is reached by (DMN-067). Required for `acme`,
    /// added as a SAN for `self_signed`, and advertised to the platform in
    /// place of the address — so the node survives an IP change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Contact address for the ACME account.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acme_email: Option<String>,
    /// ACME directory URL. Overridable so tests and staging can point at
    /// Let's Encrypt's staging environment instead of burning rate limits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acme_directory: Option<String>,
    /// Where the HTTP-01 challenge is served, for the duration of one order.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acme_http_listen: Option<String>,
}

/// Let's Encrypt production, used when `acme_directory` is unset.
pub const DEFAULT_ACME_DIRECTORY: &str = "https://acme-v02.api.letsencrypt.org/directory";

/// Where the HTTP-01 challenge listens unless configured otherwise. ACME
/// validation always connects to port 80, so this is not a free choice.
pub const DEFAULT_ACME_HTTP_LISTEN: &str = "0.0.0.0:80";

/// How the API listener terminates TLS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsMode {
    /// Plain HTTP. Only safe while the listener stays on loopback.
    #[default]
    Off,
    /// A certificate the daemon issues itself; the platform pins its
    /// fingerprint at registration, like an SSH host key.
    SelfSigned,
    /// A certificate the daemon obtains from an ACME provider (Let's
    /// Encrypt) for its `domain` and renews on its own. The chain is
    /// publicly trusted, so the platform verifies it normally instead of
    /// pinning a fingerprint.
    Acme,
    /// Operator-supplied certificate and key, e.g. renewed by certbot.
    Files,
}

impl TlsMode {
    /// The value as it appears in config.toml — also what the daemon reports
    /// to the platform, so the two never drift apart.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::SelfSigned => "self_signed",
            Self::Acme => "acme",
            Self::Files => "files",
        }
    }
}

impl std::fmt::Display for TlsMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Default path of the daemon's local API socket (see `ApiConfig::socket`).
pub const DEFAULT_API_SOCKET: &str = "/run/asc/asc.sock";

impl ApiConfig {
    /// Reject a `[api]` section that cannot work, before the listener is
    /// bound rather than at the first handshake. Checked at startup and by
    /// `asc api tls`, so a bad setting is refused where it is entered.
    pub fn validate(&self) -> Result<()> {
        match self.tls {
            TlsMode::Off => {}
            TlsMode::SelfSigned => {}
            // The mode exists end to end everywhere else — the platform
            // stores it, verifies the chain instead of pinning a fingerprint,
            // and dials the domain — but the daemon does not obtain the
            // certificate itself yet (DMN-067). An operator reaches the same
            // result today with certbot and `files`, so this refuses at
            // startup rather than binding a listener with nothing behind it.
            TlsMode::Acme => bail!(
                "api.tls = \"acme\" is not available yet: obtain the certificate with \
                 certbot and set api.tls = \"files\" with api.tls_cert and api.tls_key"
            ),
            TlsMode::Files => {
                if self.tls_cert.is_none() {
                    bail!("api.tls = \"files\" requires api.tls_cert");
                }
                if self.tls_key.is_none() {
                    bail!("api.tls = \"files\" requires api.tls_key");
                }
            }
        }
        if let Some(domain) = self.domain.as_deref()
            && !domain.is_empty()
            && !valid_domain(domain)
        {
            bail!("api.domain is not a valid host name: {domain}");
        }
        Ok(())
    }

    /// Whether the listener stays on the machine itself.
    pub fn is_loopback(&self) -> bool {
        self.listen.starts_with("127.")
            || self.listen.starts_with("localhost")
            || self.listen.starts_with("[::1]")
    }
}

/// The conservative subset a certificate can be issued for: DNS labels, no
/// scheme, no port, no path. The value ends up in a dial address and in an
/// ACME order, so being generous here would be the wrong kind of helpful.
pub fn valid_domain(value: &str) -> bool {
    if value.is_empty() || value.len() > 253 {
        return false;
    }
    let labels: Vec<&str> = value.split('.').collect();
    if labels.len() < 2 {
        return false;
    }
    labels.iter().all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    })
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8420".into(),
            socket: PathBuf::from(DEFAULT_API_SOCKET),
            token: None,
            tls: TlsMode::Off,
            tls_cert: None,
            tls_key: None,
            tls_sans: Vec::new(),
            domain: None,
            acme_email: None,
            acme_directory: None,
            acme_http_listen: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    /// Default log level (`trace`..`error`); `RUST_LOG` overrides it.
    /// Toggled between `info` and `debug` by `asc config debug`.
    pub level: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    /// Directory for daemon state (backups, registries cache, ...).
    pub data_dir: PathBuf,
    /// Root of app directories: `<apps_dir>/<id>/` (see app-management.md).
    pub apps_dir: PathBuf,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            data_dir: scoped_path("data", DEFAULT_DATA_DIR),
            apps_dir: scoped_path("apps", DEFAULT_APPS_DIR),
        }
    }
}

/// `[updater]` — settings chosen at install time, managed by `asc-updater`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdaterConfig {
    /// Automatic update checks (systemd timer).
    pub enabled: bool,
    pub channel: Channel,
    /// Daily check time, `HH:MM` (systemd `OnCalendar`).
    pub schedule: String,
    /// Where the `asc` binary is installed.
    pub install_dir: PathBuf,
}

impl Default for UpdaterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            channel: Channel::Stable,
            schedule: "04:00".into(),
            install_dir: scoped_path("bin", DEFAULT_INSTALL_DIR),
        }
    }
}

const DEFAULT_INSTALL_DIR: &str = "/usr/local/bin";

/// Update channel: stable releases or beta (pre-releases included).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    #[default]
    Stable,
    Beta,
}

impl std::str::FromStr for Channel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "stable" => Ok(Channel::Stable),
            "beta" => Ok(Channel::Beta),
            other => Err(format!(
                "unknown channel '{other}', expected 'stable' or 'beta'"
            )),
        }
    }
}

impl std::fmt::Display for Channel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Channel::Stable => "stable",
            Channel::Beta => "beta",
        })
    }
}

impl Config {
    /// Effective config file path: `$ASC_CONFIG`, then `~/.asc/config.toml`
    /// for regular users, `/etc/asc/config.toml` for root.
    pub fn path() -> PathBuf {
        std::env::var_os("ASC_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| scoped_path("config.toml", DEFAULT_CONFIG_PATH))
    }

    /// Load the config, falling back to defaults when the file does not exist.
    pub fn load() -> anyhow::Result<Self> {
        let mut config = Self::load_from(&Self::path())?;
        // The root-managed [policy] always comes from the system config
        // (world-readable 0644) — a regular user cannot lift it by editing
        // their own ~/.asc/config.toml. $ASC_CONFIG (tests, development)
        // opts out of the override.
        if !is_root() && std::env::var_os("ASC_CONFIG").is_none() {
            config.policy = Self::load_from(Path::new(DEFAULT_CONFIG_PATH))
                .map(|system| system.policy)
                .unwrap_or_default();
        }
        Ok(config)
    }

    pub fn load_from(path: &Path) -> anyhow::Result<Self> {
        match fs::read_to_string(path) {
            Ok(raw) => toml::from_str(&raw)
                .with_context(|| format!("invalid config file {}", path.display())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            // Pre-split installs kept config.toml root-only (0600). Regular
            // users fall back to defaults until the daemon migrates the file
            // to 0644 on its next start — better than breaking every command.
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("cannot read config file {}", path.display())),
        }
    }

    /// Persist the config to the effective path.
    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to(&Self::path())
    }

    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            fs::create_dir_all(dir)
                .with_context(|| format!("cannot create config directory {}", dir.display()))?;
        }
        let raw = toml::to_string_pretty(self).context("cannot serialize config")?;
        fs::write(path, raw)
            .with_context(|| format!("cannot write config file {}", path.display()))?;
        // World-readable: regular users need the language and [policy]
        // settings. Secrets (API token, platform tokens) live in separate
        // root-only files, never here.
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o644)).with_context(|| {
                format!("cannot set permissions on config file {}", path.display())
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::i18n::Lang;

    #[test]
    fn missing_file_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load_from(&dir.path().join("nope.toml")).unwrap();
        assert_eq!(cfg.language, Lang::En);
        assert_eq!(cfg.log.level, "info");
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("etc/asc/config.toml");
        let cfg = Config {
            language: Lang::Ru,
            ..Config::default()
        };
        cfg.save_to(&path).unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded.language, Lang::Ru);
        assert_eq!(loaded.daemon.data_dir, cfg.daemon.data_dir);
    }

    #[test]
    fn unknown_fields_are_ignored_for_forward_compatibility() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "language = \"ru\"\nfuture_option = true\n").unwrap();
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.language, Lang::Ru);
    }

    #[test]
    fn non_root_defaults_live_under_home() {
        // Under root (some CI containers) the defaults are the system paths;
        // the per-user tree only exists for regular users.
        if is_root() {
            return;
        }
        let base = user_base_dir().expect("$HOME is set in tests");
        let config = Config::default();
        assert_eq!(config.daemon.data_dir, base.join("data"));
        assert_eq!(config.daemon.apps_dir, base.join("apps"));
        assert_eq!(config.updater.install_dir, base.join("bin"));
    }

    #[test]
    fn invalid_config_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "language = \"klingon\"").unwrap();
        assert!(Config::load_from(&path).is_err());
    }
}

#[cfg(test)]
mod api_validation_tests {
    use super::*;

    fn direct() -> ApiConfig {
        ApiConfig {
            listen: "0.0.0.0:8420".into(),
            tls: TlsMode::SelfSigned,
            ..ApiConfig::default()
        }
    }

    #[test]
    fn the_default_configuration_is_valid() {
        assert!(ApiConfig::default().validate().is_ok());
    }

    #[test]
    fn a_configuration_from_before_this_release_still_parses() {
        // An existing config.toml carries none of the new keys; the daemon
        // must come up on it untouched.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[api]
listen = \"127.0.0.1:8420\"
tls = \"self_signed\"
",
        )
        .unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded.api.tls, TlsMode::SelfSigned);
        assert!(loaded.api.domain.is_none());
        assert!(loaded.api.validate().is_ok());
    }

    #[test]
    fn files_mode_needs_both_halves_of_the_pair() {
        let mut config = direct();
        config.tls = TlsMode::Files;
        assert!(config.validate().is_err());
        config.tls_cert = Some(PathBuf::from("/etc/asc/api.crt"));
        assert!(config.validate().is_err());
        config.tls_key = Some(PathBuf::from("/etc/asc/api.key"));
        assert!(config.validate().is_ok());
    }

    #[test]
    fn acme_is_refused_until_the_daemon_can_issue_certificates() {
        // Better a clear refusal at startup than a listener with nothing
        // behind it (DMN-067).
        let mut config = direct();
        config.tls = TlsMode::Acme;
        config.domain = Some("node.example.com".into());
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("acme"), "{err}");
        assert!(
            err.contains("files"),
            "the message must point at the way that works: {err}"
        );
    }

    #[test]
    fn a_malformed_domain_is_rejected_wherever_it_appears() {
        let mut config = direct();
        config.domain = Some("https://node.example.com/x".into());
        assert!(config.validate().is_err());
        config.domain = Some("node.example.com".into());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn loopback_is_recognised_for_every_spelling() {
        for listen in ["127.0.0.1:8420", "localhost:8420", "[::1]:8420"] {
            let config = ApiConfig {
                listen: listen.into(),
                ..ApiConfig::default()
            };
            assert!(config.is_loopback(), "{listen}");
        }
        assert!(!direct().is_loopback());
    }

    #[test]
    fn domains_are_host_names_and_nothing_else() {
        for good in ["node.example.com", "a-b.example.co.uk", "n1.eu"] {
            assert!(valid_domain(good), "{good}");
        }
        // No scheme, no port, no path, no single label, no stray dots: the
        // value ends up in a dial address and in a certificate.
        for bad in [
            "",
            "localhost",
            "node.example.com:8420",
            "https://node.example.com",
            "node.example.com/path",
            "-node.example.com",
            "node-.example.com",
            "node..com",
            "node example.com",
            "NODE.example.com",
        ] {
            assert!(!valid_domain(bad), "{bad}");
        }
    }
}
