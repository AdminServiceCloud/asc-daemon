//! The node's web server (DMN-122..DMN-125): nginx installed in `system` or
//! `docker` mode, virtual hosts ("sites") proxying to apps or addresses,
//! Let's Encrypt certificates issued and renewed by the daemon, provided
//! certificates, and Cloudflare real IP. See docs/english/webserver.md.
//!
//! State lives in three files, all rewritten whole under [`WebServer`]'s
//! lock: settings in `/etc/asc/webserver/webserver.toml`, sites in
//! `/var/lib/asc/webserver/sites.json` (0600 — provided keys), statuses in
//! `/var/lib/asc/webserver/status.json`. Every change goes through
//! [`apply`]: render into a staging directory, `nginx -t`, swap, reload.
//! Only the root daemon manages a web server.

pub mod acme;
pub mod apply;
pub mod cert;
pub mod cloudflare;
pub mod engine;
pub mod health;
pub mod model;
pub mod render;

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::daemon::config::{self, Config};
use model::{Mode, Settings, Site, SiteState, SiteStatus, SiteView, TlsMode, TlsState};
use render::Paths;

/// Renew a certificate once it has less than this left.
pub const RENEW_BEFORE_SECS: i64 = 30 * 24 * 3600;
/// How often the background pass looks for due work (renewals, Cloudflare
/// refresh, moved app ports).
const TICK: Duration = Duration::from_secs(600);
const CLOUDFLARE_REFRESH_SECS: i64 = 24 * 3600;

const SETTINGS_FILE: &str = "webserver.toml";
const SITES_FILE: &str = "sites.json";
const STATUS_FILE: &str = "status.json";
const CLOUDFLARE_FILE: &str = "cloudflare.json";

pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Write through a temporary file and `rename`, so nginx never reads a
/// half-written file and a crash leaves the previous version.
pub fn write_atomic(path: &Path, content: &[u8], mode: u32) -> Result<()> {
    let dir = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let tmp = dir.join(format!(
        ".{}.asc-tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file")
    ));
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)
            .with_context(|| format!("cannot write {}", tmp.display()))?;
        file.write_all(content)
            .and_then(|()| file.sync_all())
            .with_context(|| format!("cannot write {}", tmp.display()))?;
    }
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("cannot set permissions on {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))
}

/// Per-site statuses plus the outcome of the last apply.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StatusFile {
    pub last_error: String,
    pub last_applied: i64,
    pub sites: BTreeMap<String, SiteStatus>,
}

/// What `GetWebServer` reports.
#[derive(Debug, Clone, Serialize)]
pub struct Overview {
    pub installed: bool,
    pub mode: Mode,
    pub engine: &'static str,
    pub version: Option<String>,
    pub running: bool,
    pub adopted: bool,
    pub settings: Settings,
    pub last_error: String,
    pub last_applied: i64,
    pub cloudflare_updated: i64,
    pub site_count: usize,
    pub nginx_present: bool,
    pub docker_available: bool,
}

/// The web server manager, shared by the API and the background pass.
pub struct WebServer {
    paths: Paths,
    config: Config,
    lock: Mutex<()>,
    wake: tokio::sync::Notify,
    /// Live health of upstream servers (DMN-126).
    health: health::Registry,
}

impl WebServer {
    pub fn new(config: &Config) -> Self {
        Self::with_paths(config, Paths::system())
    }

    pub fn with_paths(config: &Config, paths: Paths) -> Self {
        Self {
            paths,
            config: config.clone(),
            lock: Mutex::new(()),
            wake: tokio::sync::Notify::new(),
            health: health::Registry::default(),
        }
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    fn guard(&self) -> MutexGuard<'_, ()> {
        self.lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn require_root() -> Result<()> {
        if !config::is_root() {
            bail!("the web server is managed by the system (root) daemon only");
        }
        Ok(())
    }

    fn engine(&self, mode: Mode) -> engine::Engine<'_> {
        engine::Engine {
            mode,
            docker: &self.config.docker,
            paths: &self.paths,
        }
    }

    // ── Stores ───────────────────────────────────────────────────────────

    pub fn load_settings(&self) -> Settings {
        let path = self.paths.root.join(SETTINGS_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|err| {
                warn!(error = %err, path = %path.display(), "unreadable web server settings, using defaults");
                Settings::default()
            }),
            Err(_) => Settings::default(),
        }
    }

    fn save_settings(&self, settings: &Settings) -> Result<()> {
        let text = toml::to_string_pretty(settings).context("cannot serialize settings")?;
        let body = format!(
            "# asc-daemon web server settings — change them with `asc web` or the panel.\n{text}"
        );
        write_atomic(&self.paths.root.join(SETTINGS_FILE), body.as_bytes(), 0o600)
    }

    pub fn load_sites(&self) -> Result<Vec<Site>> {
        let path = self.paths.state.join(SITES_FILE);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("{} is corrupt", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    fn save_sites(&self, sites: &[Site]) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(sites).context("cannot serialize sites")?;
        write_atomic(&self.paths.state.join(SITES_FILE), &bytes, 0o600)
    }

    pub fn load_status(&self) -> StatusFile {
        std::fs::read(self.paths.state.join(STATUS_FILE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn save_status(&self, status: &StatusFile) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(status).context("cannot serialize status")?;
        write_atomic(&self.paths.state.join(STATUS_FILE), &bytes, 0o600)
    }

    fn cloudflare_path(&self) -> PathBuf {
        self.paths.state.join(CLOUDFLARE_FILE)
    }

    // ── Views ────────────────────────────────────────────────────────────

    pub fn overview(&self) -> Overview {
        let settings = self.load_settings();
        let status = self.load_status();
        let state = self.engine(settings.mode).state();
        let site_count = self.load_sites().map(|s| s.len()).unwrap_or(0);
        Overview {
            installed: settings.mode != Mode::None,
            mode: settings.mode,
            engine: "nginx",
            version: state.version,
            running: state.running,
            adopted: settings.host.adopted,
            last_error: status.last_error,
            last_applied: status.last_applied,
            cloudflare_updated: cloudflare::Ranges::load(&self.cloudflare_path()).updated_at,
            site_count,
            nginx_present: engine::nginx_binary().is_some(),
            docker_available: crate::daemon::docker::available(&self.config.docker),
            settings,
        }
    }

    fn view(&self, site: &Site, status: &StatusFile) -> SiteView {
        let mut st = status.sites.get(&site.id).cloned().unwrap_or_default();
        st.upstream_health = if site.upstream.health_check.enabled() {
            self.health.snapshot(&site.id, &st.upstream_addresses)
        } else {
            Vec::new()
        };
        if st.tls.state == TlsState::Active
            && st.tls.not_after > 0
            && st.tls.not_after - unix_now() < RENEW_BEFORE_SECS
        {
            st.tls.state = TlsState::Expiring;
        }
        SiteView {
            site: site.redacted(),
            status: st,
        }
    }

    pub fn sites(&self, managed_by: Option<&str>) -> Result<Vec<SiteView>> {
        let status = self.load_status();
        Ok(self
            .load_sites()?
            .iter()
            .filter(|s| managed_by.is_none_or(|m| s.managed_by.as_deref() == Some(m)))
            .map(|s| self.view(s, &status))
            .collect())
    }

    pub fn site(&self, id: &str) -> Result<SiteView> {
        let status = self.load_status();
        let sites = self.load_sites()?;
        let site = sites
            .iter()
            .find(|s| s.id == id)
            .with_context(|| format!("site {id:?} not found"))?;
        Ok(self.view(site, &status))
    }

    /// Files as nginx currently reads them.
    pub fn files(&self) -> Result<Vec<(String, String)>> {
        let settings = self.load_settings();
        if settings.mode == Mode::None {
            bail!("the web server is not installed");
        }
        let mut out = Vec::new();
        let main = engine::main_conf_path(settings.mode, &self.paths);
        out.push((
            main.display().to_string(),
            std::fs::read_to_string(&main).unwrap_or_default(),
        ));
        for rel in [
            render::CUSTOM_HTTP,
            render::CLOUDFLARE,
            render::ACME_SNIPPET,
            render::PROXY_SNIPPET,
        ] {
            let path = self.paths.root.join(rel);
            if let Ok(text) = std::fs::read_to_string(&path) {
                out.push((path.display().to_string(), text));
            }
        }
        let mut sites: Vec<PathBuf> = std::fs::read_dir(self.paths.root.join("sites"))
            .map(|entries| entries.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        sites.sort();
        for path in sites {
            if path.extension().is_some_and(|e| e == "conf")
                && let Ok(text) = std::fs::read_to_string(&path)
            {
                out.push((path.display().to_string(), text));
            }
        }
        Ok(out)
    }

    // ── Operations ───────────────────────────────────────────────────────

    pub fn install(&self, mode: Mode, progress: engine::Progress<'_>) -> Result<Overview> {
        Self::require_root()?;
        if mode == Mode::None {
            bail!("choose a mode: system or docker");
        }
        let _guard = self.guard();
        let mut settings = self.load_settings();
        if settings.mode != Mode::None && settings.mode != mode {
            bail!(
                "the web server is already installed in {} mode — uninstall it first",
                settings.mode.label()
            );
        }
        for dir in [&self.paths.root, &self.paths.state] {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
        }
        std::fs::create_dir_all(self.paths.webroot.join(".well-known/acme-challenge"))
            .with_context(|| format!("cannot create {}", self.paths.webroot.display()))?;
        set_mode(&self.paths.webroot, 0o755);
        set_mode(&self.paths.webroot.join(".well-known"), 0o755);
        set_mode(
            &self.paths.webroot.join(".well-known/acme-challenge"),
            0o755,
        );

        settings.mode = mode;
        let sites = self.load_sites()?;
        let mut status = self.load_status();
        if mode == Mode::Docker {
            // The container starts on the generated configuration, so it has
            // to exist before the container does.
            self.apply_locked(&settings, &sites, &mut status, apply::Stage::WriteOnly)?;
        }
        self.engine(mode).install(&mut settings, progress)?;
        self.save_settings(&settings)?;
        progress("applying the configuration");
        let result = self.apply_locked(&settings, &sites, &mut status, apply::Stage::Full);
        self.save_status(&status)?;
        result?;
        progress("the web server is ready");
        self.wake.notify_one();
        Ok(self.overview())
    }

    pub fn uninstall(&self, purge: bool, progress: engine::Progress<'_>) -> Result<()> {
        Self::require_root()?;
        let _guard = self.guard();
        let settings = self.load_settings();
        self.engine(settings.mode).uninstall(&settings, progress)?;
        let mut cleared = settings.clone();
        cleared.mode = Mode::None;
        cleared.host = Default::default();
        if purge {
            for dir in [&self.paths.root, &self.paths.state, &self.paths.webroot] {
                let _ = std::fs::remove_dir_all(dir);
            }
            progress("removed configuration, sites and certificates");
        } else {
            self.save_settings(&cleared)?;
        }
        Ok(())
    }

    /// Validates and applies new node-wide settings; nothing is saved when
    /// nginx refuses them. Mode and host profile are not the caller's to set.
    pub fn update_settings(&self, mut new: Settings) -> Result<Overview> {
        Self::require_root()?;
        new.validate()?;
        let _guard = self.guard();
        let current = self.load_settings();
        new.mode = current.mode;
        new.host = current.host.clone();
        if current.mode == Mode::None {
            self.save_settings(&new)?;
            return Ok(self.overview());
        }
        let sites = self.load_sites()?;
        let mut status = self.load_status();
        let result = self.apply_locked(&new, &sites, &mut status, apply::Stage::Full);
        self.save_status(&status)?;
        result?;
        self.save_settings(&new)?;
        Ok(self.overview())
    }

    /// `nginx -t` of what would be applied now, without applying it.
    pub fn test(&self) -> Result<(bool, String)> {
        Self::require_root()?;
        let _guard = self.guard();
        let settings = self.load_settings();
        if settings.mode == Mode::None {
            bail!("the web server is not installed");
        }
        let sites = self.load_sites()?;
        let mut status = self.load_status();
        match self.apply_locked(&settings, &sites, &mut status, apply::Stage::TestOnly) {
            Ok(output) => Ok((true, output)),
            Err(err) => Ok((false, format!("{err:#}"))),
        }
    }

    /// Re-render everything, test and reload.
    pub fn reload(&self) -> Result<()> {
        Self::require_root()?;
        let _guard = self.guard();
        self.reapply_locked()
    }

    fn reapply_locked(&self) -> Result<()> {
        let settings = self.load_settings();
        let sites = self.load_sites()?;
        let mut status = self.load_status();
        let result = self.apply_locked(&settings, &sites, &mut status, apply::Stage::Full);
        self.save_status(&status)?;
        result.map(|_| ())
    }

    /// Replaces every site carrying `managed_by` with `incoming`.
    pub fn replace_sites(&self, managed_by: &str, incoming: Vec<Site>) -> Result<Vec<SiteView>> {
        Self::require_root()?;
        if managed_by.trim().is_empty() {
            bail!("managed_by is required");
        }
        let mut incoming = incoming;
        let mut ids = HashSet::new();
        for site in &mut incoming {
            site.managed_by = Some(managed_by.to_string());
            site.normalize()?;
            if !ids.insert(site.id.clone()) {
                bail!("site id {:?} appears twice", site.id);
            }
        }
        let _guard = self.guard();
        let current = self.load_sites()?;
        if let Some(clash) = current
            .iter()
            .find(|s| s.managed_by.as_deref() != Some(managed_by) && ids.contains(&s.id))
        {
            bail!("site id {:?} already belongs to another owner", clash.id);
        }
        let mut next: Vec<Site> = current
            .into_iter()
            .filter(|s| s.managed_by.as_deref() != Some(managed_by))
            .collect();
        next.extend(incoming);
        self.commit_sites(next)?;
        self.sites(Some(managed_by))
    }

    pub fn upsert_site(&self, mut site: Site) -> Result<SiteView> {
        Self::require_root()?;
        site.normalize()?;
        let _guard = self.guard();
        let mut sites = self.load_sites()?;
        if let Some(existing) = sites.iter_mut().find(|s| s.id == site.id) {
            if existing.managed_by != site.managed_by {
                bail!(
                    "site {:?} is managed by {}",
                    site.id,
                    existing.managed_by.as_deref().unwrap_or("local")
                );
            }
            *existing = site.clone();
        } else {
            sites.push(site.clone());
        }
        self.commit_sites(sites)?;
        self.site(&site.id)
    }

    pub fn remove_site(&self, id: &str) -> Result<bool> {
        Self::require_root()?;
        let _guard = self.guard();
        let mut sites = self.load_sites()?;
        let before = sites.len();
        sites.retain(|s| s.id != id);
        if sites.len() == before {
            return Ok(false);
        }
        self.commit_sites(sites)?;
        Ok(true)
    }

    /// Saves the site list and applies it. The list is saved even when the
    /// apply fails: it is the desired state, and each site's status says
    /// what nginx made of it.
    fn commit_sites(&self, sites: Vec<Site>) -> Result<()> {
        self.save_sites(&sites)?;
        let settings = self.load_settings();
        let mut status = self.load_status();
        let ids: HashSet<&str> = sites.iter().map(|s| s.id.as_str()).collect();
        status.sites.retain(|id, _| ids.contains(id.as_str()));
        let result = if settings.mode == Mode::None {
            for site in &sites {
                let st = status.sites.entry(site.id.clone()).or_default();
                st.state = SiteState::Pending;
                st.message = "the web server is not installed on this node".into();
            }
            Ok(String::new())
        } else {
            self.apply_locked(&settings, &sites, &mut status, apply::Stage::Full)
        };
        self.save_status(&status)?;
        if sites.iter().any(|s| s.tls.mode == TlsMode::Acme) {
            self.wake.notify_one();
        }
        result.map(|_| ())
    }

    /// A site's file as it would be rendered, without storing anything.
    pub fn render_site(&self, mut site: Site) -> Result<String> {
        site.normalize()?;
        let settings = self.load_settings();
        let status = self.load_status();
        let features = self.features(&settings);
        let resolved = self.resolve(&site, &status)?;
        Ok(render::render_site(
            &resolved,
            &settings,
            features,
            &self.paths.root,
        ))
    }

    /// Orders a certificate for one site now, ignoring backoff.
    pub fn renew(&self, id: &str) -> Result<SiteView> {
        Self::require_root()?;
        let site = self
            .load_sites()?
            .into_iter()
            .find(|s| s.id == id)
            .with_context(|| format!("site {id:?} not found"))?;
        if site.tls.mode != TlsMode::Acme {
            bail!("site {id:?} does not use Let's Encrypt");
        }
        self.issue(&site);
        self.site(id)
    }

    // ── Background pass ─────────────────────────────────────────────────

    /// One background pass: refresh Cloudflare ranges, follow moved app
    /// ports, renew due certificates.
    pub fn tick(&self) {
        if !config::is_root() {
            return;
        }
        let settings = self.load_settings();
        if settings.mode == Mode::None {
            return;
        }
        let mut changed = self.refresh_cloudflare();
        changed |= self.upstreams_moved();
        if changed {
            let _guard = self.guard();
            if let Err(err) = self.reapply_locked() {
                warn!(error = %format!("{err:#}"), "web server re-apply failed");
            }
        }
        self.acme_pass();
    }

    fn refresh_cloudflare(&self) -> bool {
        let path = self.cloudflare_path();
        let current = cloudflare::Ranges::load(&path);
        let now = unix_now();
        if now - current.updated_at < CLOUDFLARE_REFRESH_SECS {
            return false;
        }
        match cloudflare::download(now) {
            Ok(fresh) => {
                let changed = fresh.cidrs != current.cidrs;
                if let Ok(bytes) = serde_json::to_vec_pretty(&fresh) {
                    let _ = write_atomic(&path, &bytes, 0o644);
                }
                if changed {
                    info!(ranges = fresh.cidrs.len(), "Cloudflare ranges changed");
                }
                changed
            }
            Err(err) => {
                warn!(error = %format!("{err:#}"), "cannot refresh Cloudflare ranges");
                false
            }
        }
    }

    /// Whether any applied site's upstream now resolves differently — an
    /// app recreated with another host port.
    fn upstreams_moved(&self) -> bool {
        let Ok(sites) = self.load_sites() else {
            return false;
        };
        let status = self.load_status();
        sites
            .iter()
            .filter(|s| !s.disabled && s.raw_config.is_none())
            .any(|site| {
                let known = status.sites.get(&site.id).map(|s| &s.upstream_addresses);
                match self.resolve(site, &status) {
                    Ok(resolved) => known != Some(&resolved.addresses),
                    Err(_) => false,
                }
            })
    }

    fn acme_pass(&self) {
        let Ok(sites) = self.load_sites() else {
            return;
        };
        let now = unix_now();
        for site in sites
            .iter()
            .filter(|s| !s.disabled && s.tls.mode == TlsMode::Acme)
        {
            let status = self.load_status();
            let st = status.sites.get(&site.id).cloned().unwrap_or_default();
            if st.state != SiteState::Applied || st.tls.next_attempt > now {
                continue;
            }
            let has_cert = self.acme_material(&site.id).is_some();
            let due = !has_cert
                || st.tls.names != site.server_names
                || st.tls.not_after - now < RENEW_BEFORE_SECS;
            if due {
                self.issue(site);
            }
        }
    }

    /// Issued certificate files of a site, when both exist.
    fn acme_material(&self, id: &str) -> Option<(PathBuf, PathBuf)> {
        let dir = self.paths.acme_cert_dir(id);
        let (cert, key) = (dir.join("fullchain.pem"), dir.join("privkey.pem"));
        (cert.is_file() && key.is_file()).then_some((cert, key))
    }

    /// Runs one ACME order for `site` and records the outcome; on success
    /// the site is re-rendered with HTTPS.
    fn issue(&self, site: &Site) {
        let settings = self.load_settings();
        {
            let _guard = self.guard();
            let mut status = self.load_status();
            let st = status.sites.entry(site.id.clone()).or_default();
            st.tls.state = TlsState::Issuing;
            st.tls.last_error.clear();
            let _ = self.save_status(&status);
        }
        info!(site = %site.id, names = ?site.server_names, "requesting a certificate");
        let outcome = acme::issue(&settings, &self.paths, &site.server_names);
        let _guard = self.guard();
        let mut status = self.load_status();
        let now = unix_now();
        let st = status.sites.entry(site.id.clone()).or_default();
        match outcome {
            Ok(issued) => {
                let dir = self.paths.acme_cert_dir(&site.id);
                let written =
                    write_atomic(&dir.join("privkey.pem"), issued.key_pem.as_bytes(), 0o600)
                        .and_then(|()| {
                            write_atomic(
                                &dir.join("fullchain.pem"),
                                issued.chain_pem.as_bytes(),
                                0o644,
                            )
                        });
                match written.and_then(|()| cert::inspect(&issued.chain_pem)) {
                    Ok(info) => {
                        st.tls.state = TlsState::Active;
                        st.tls.not_after = info.not_after;
                        st.tls.issuer = info.issuer;
                        st.tls.names = site.server_names.clone();
                        st.tls.failures = 0;
                        st.tls.next_attempt = 0;
                        st.tls.last_error.clear();
                        info!(site = %site.id, "certificate issued");
                    }
                    Err(err) => fail(st, now, TlsState::Error, format!("{err:#}")),
                }
            }
            Err(acme::IssueError::Dns(message)) => fail(st, now, TlsState::PendingDns, message),
            Err(acme::IssueError::Acme(message)) => fail(st, now, TlsState::Error, message),
        }
        let _ = self.save_status(&status);
        if let Err(err) = self.reapply_locked() {
            warn!(error = %format!("{err:#}"), "web server re-apply after issuance failed");
        }
    }
}

fn fail(st: &mut SiteStatus, now: i64, state: TlsState, message: String) {
    warn!(%message, "certificate request failed");
    st.tls.state = state;
    st.tls.last_error = message;
    st.tls.failures = st.tls.failures.saturating_add(1);
    let backoff = 3600i64 << st.tls.failures.saturating_sub(1).min(5);
    st.tls.next_attempt = now + backoff.min(24 * 3600);
}

fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

impl WebServer {
    /// Every upstream server a health check covers: applied sites with a
    /// check, paired with the addresses they resolved to.
    fn health_targets(&self) -> Vec<health::Target> {
        let Ok(sites) = self.load_sites() else {
            return Vec::new();
        };
        let status = self.load_status();
        let mut targets = Vec::new();
        for site in sites
            .iter()
            .filter(|s| !s.disabled && s.raw_config.is_none() && s.upstream.health_check.enabled())
        {
            let Some(st) = status.sites.get(&site.id) else {
                continue;
            };
            if st.state != SiteState::Applied {
                continue;
            }
            for address in &st.upstream_addresses {
                targets.push(health::Target {
                    site: site.id.clone(),
                    address: address.clone(),
                    check: site.upstream.health_check.clone(),
                    tls: site.upstream.tls,
                });
            }
        }
        targets
    }

    /// One TCP connect from this node (ProbeTcp): (reachable, latency, error).
    pub fn probe_tcp(&self, address: &str, timeout_ms: u32) -> (bool, u32, String) {
        let timeout = Duration::from_millis(u64::from(if timeout_ms == 0 {
            3000
        } else {
            timeout_ms.min(30_000)
        }));
        match health::probe_tcp(address, timeout) {
            Ok(latency) => (true, latency, String::new()),
            Err(error) => (false, 0, error),
        }
    }
}

/// Minimum spacing of re-applies triggered by health changes — a flapping
/// server must not reload nginx every second.
const HEALTH_REAPPLY_DEBOUNCE: Duration = Duration::from_secs(3);

/// The health-check loop (DMN-126): once a second, probe what is due; when a
/// server flips in or out of rotation, re-render and reload (debounced).
async fn health_loop(web: Arc<WebServer>) {
    let mut pending = false;
    let mut last_apply = std::time::Instant::now() - HEALTH_REAPPLY_DEBOUNCE;
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let scan = Arc::clone(&web);
        let Ok(targets) = tokio::task::spawn_blocking(move || scan.health_targets()).await else {
            continue;
        };
        let keep: HashSet<health::Key> = targets
            .iter()
            .map(|t| (t.site.clone(), t.address.clone()))
            .collect();
        web.health.retain(&keep);
        let due = web.health.due(&targets, std::time::Instant::now());
        let probes = due.into_iter().map(|target| {
            let web = Arc::clone(&web);
            tokio::task::spawn_blocking(move || {
                let result = health::probe(&target);
                web.health
                    .record(&target, result, std::time::Instant::now())
            })
        });
        for flipped in futures_util::future::join_all(probes).await {
            pending |= flipped.unwrap_or(false);
        }
        if pending && last_apply.elapsed() >= HEALTH_REAPPLY_DEBOUNCE {
            pending = false;
            last_apply = std::time::Instant::now();
            let apply = Arc::clone(&web);
            let _ = tokio::task::spawn_blocking(move || {
                let _guard = apply.guard();
                if let Err(err) = apply.reapply_locked() {
                    warn!(error = %format!("{err:#}"), "re-apply after a health change failed");
                } else {
                    info!("upstream health changed, configuration re-applied");
                }
            })
            .await;
        }
    }
}

/// Starts the background pass on the daemon runtime.
pub fn start(web: Arc<WebServer>) {
    if !config::is_root() {
        return;
    }
    tokio::spawn(health_loop(Arc::clone(&web)));
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        loop {
            let pass = Arc::clone(&web);
            if let Err(err) = tokio::task::spawn_blocking(move || pass.tick()).await {
                warn!(error = %err, "web server background pass panicked");
            }
            tokio::select! {
                () = tokio::time::sleep(TICK) => {}
                () = web.wake.notified() => {}
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        let mut st = SiteStatus::default();
        fail(&mut st, 0, TlsState::Error, "x".into());
        assert_eq!(st.tls.next_attempt, 3600);
        fail(&mut st, 0, TlsState::Error, "x".into());
        assert_eq!(st.tls.next_attempt, 7200);
        for _ in 0..10 {
            fail(&mut st, 0, TlsState::Error, "x".into());
        }
        assert_eq!(st.tls.next_attempt, 24 * 3600);
    }

    #[test]
    fn atomic_write_sets_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b.pem");
        write_atomic(&path, b"secret", 0o600).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"secret");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
