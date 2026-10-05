//! The node's fail2ban (DMN-150): install it, own one file of its
//! configuration (`/etc/fail2ban/jail.d/asc.local`), switch the known jails
//! on and off, and list, add and release bans through `fail2ban-client`. Bans
//! are enforced in fail2ban's own nftables table, next to the firewall
//! module's `inet asc`. See docs/english/fail2ban.md. Only the root daemon
//! manages fail2ban.
//!
//! The model lives in `/var/lib/asc/fail2ban/state.json`; every change goes
//! through [`Fail2ban::apply_config`]: render, write (keeping the previous
//! file), `fail2ban-client -t`, and put the previous file back if fail2ban
//! refuses the new one.

pub mod client;
pub mod config;
pub mod model;

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::daemon::exec::{self, Progress, invalid, precondition, remove_package, run_streaming};
use crate::daemon::firewall::host::Facts;
use crate::daemon::webserver::write_atomic;
use client::{Ban, Client, JailStatus, SystemClient};
use model::{JailConfig, KNOWN_JAILS, Model, Settings};

/// How long jail statuses are reused: `fail2ban-client` is a Python start-up
/// per call, and a UI polls.
const STATUS_TTL: Duration = Duration::from_secs(4);

#[derive(Debug, Clone)]
pub struct Paths {
    /// `/etc/fail2ban/jail.d`.
    pub jail_dir: PathBuf,
    /// `/var/lib/asc/fail2ban`.
    pub state: PathBuf,
}

impl Paths {
    pub fn system() -> Self {
        Self {
            jail_dir: PathBuf::from("/etc/fail2ban/jail.d"),
            state: PathBuf::from("/var/lib/asc/fail2ban"),
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.jail_dir.join(config::FILE_NAME)
    }

    fn state_file(&self) -> PathBuf {
        self.state.join("state.json")
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct State {
    model: Model,
    last_error: String,
}

/// One jail as the API shows it.
#[derive(Debug, Clone, Serialize)]
pub struct JailView {
    #[serde(flatten)]
    pub config: JailConfig,
    /// Needs the web server module, which is not installed — the jail cannot
    /// be switched on.
    pub available: bool,
    pub needs_web: bool,
    /// fail2ban is running this jail right now.
    pub active: bool,
    pub status: Option<JailStatus>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Overview {
    pub installed: bool,
    pub running: bool,
    pub version: String,
    pub settings: Settings,
    pub jails: Vec<JailView>,
    pub last_error: String,
}

/// Running jails with their status, and when they were read.
type StatusCache = (Instant, Vec<(String, JailStatus)>);

pub struct Fail2ban {
    paths: Paths,
    client: Box<dyn Client>,
    lock: Mutex<()>,
    cache: Mutex<Option<StatusCache>>,
}

impl Fail2ban {
    pub fn new() -> Self {
        Self::with(Paths::system(), Box::new(SystemClient))
    }

    pub fn with(paths: Paths, client: Box<dyn Client>) -> Self {
        Self {
            paths,
            client,
            lock: Mutex::new(()),
            cache: Mutex::new(None),
        }
    }

    fn guard(&self) -> MutexGuard<'_, ()> {
        self.lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn load_state(&self) -> State {
        std::fs::read_to_string(self.paths.state_file())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn save_state(&self, state: &State) -> Result<()> {
        let text = serde_json::to_string_pretty(state).context("cannot encode the state")?;
        write_atomic(&self.paths.state_file(), text.as_bytes(), 0o600)
            .context("cannot save the fail2ban state")
    }

    fn forget_statuses(&self) {
        *self.cache.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    fn require_installed(&self) -> Result<()> {
        if self.client.installed() {
            Ok(())
        } else {
            Err(precondition(
                "fail2ban is not installed on this node (run `asc fail2ban install`)",
            ))
        }
    }

    // ── Reading ─────────────────────────────────────────────────────────────

    pub fn overview(&self, facts: &Facts) -> Result<Overview> {
        let _guard = self.guard();
        Ok(self.overview_locked(facts))
    }

    fn overview_locked(&self, facts: &Facts) -> Overview {
        let state = self.load_state();
        let installed = self.client.installed();
        let running = installed && self.client.running();
        let statuses = if running { self.statuses() } else { Vec::new() };
        let jails = KNOWN_JAILS
            .iter()
            .map(|known| {
                let found = statuses.iter().find(|(name, _)| name == known.name);
                JailView {
                    config: state.model.jail(known.name),
                    available: !known.needs_web || facts.web_installed,
                    needs_web: known.needs_web,
                    active: found.is_some(),
                    status: found.map(|(_, s)| s.clone()),
                }
            })
            .collect();
        Overview {
            installed,
            running,
            version: if installed {
                self.client.version()
            } else {
                String::new()
            },
            settings: state.model.settings,
            jails,
            last_error: state.last_error,
        }
    }

    /// The running jails with their status, reused for [`STATUS_TTL`].
    fn statuses(&self) -> Vec<(String, JailStatus)> {
        let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((at, list)) = cache.as_ref()
            && at.elapsed() < STATUS_TTL
        {
            return list.clone();
        }
        let list: Vec<(String, JailStatus)> = self
            .client
            .jails()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|name| {
                let status = self.client.jail_status(&name).ok()?;
                Some((name, status))
            })
            .collect();
        *cache = Some((Instant::now(), list.clone()));
        list
    }

    pub fn bans(&self, jail: Option<&str>) -> Result<Vec<Ban>> {
        let _guard = self.guard();
        self.require_installed()?;
        let names: Vec<String> = match jail {
            Some(name) => {
                if model::known(name).is_none() {
                    return Err(invalid(format!("unknown jail '{name}'")));
                }
                vec![name.to_string()]
            }
            None => self.client.jails().unwrap_or_default(),
        };
        let mut out = Vec::new();
        for name in names {
            out.extend(self.client.bans(&name)?);
        }
        out.sort_by(|a, b| a.jail.cmp(&b.jail).then(a.ip.cmp(&b.ip)));
        Ok(out)
    }

    // ── Installing ──────────────────────────────────────────────────────────

    pub fn install(&self, facts: &Facts, progress: Progress<'_>) -> Result<Overview> {
        let _guard = self.guard();
        if self.client.installed() {
            progress("fail2ban is already installed");
        } else {
            exec::install_package("fail2ban", progress)?;
            if !self.client.installed() {
                return Err(precondition(
                    "the fail2ban package installed, but `fail2ban-client` is not on PATH",
                ));
            }
        }
        progress("writing the configuration");
        let mut state = self.load_state();
        state.last_error = self.apply_config(&state, facts)?.unwrap_or_default();
        self.save_state(&state)?;
        Ok(self.overview_locked(facts))
    }

    pub fn uninstall(&self, purge: bool, progress: Progress<'_>) -> Result<()> {
        let _guard = self.guard();
        self.require_installed()?;
        let _ = run_streaming("systemctl", &["disable", "--now", "fail2ban"], progress);
        remove_package("fail2ban", purge, progress)?;
        let _ = std::fs::remove_file(self.paths.config_file());
        if purge {
            let _ = std::fs::remove_dir_all(&self.paths.state);
        }
        self.forget_statuses();
        Ok(())
    }

    // ── Editing ─────────────────────────────────────────────────────────────

    pub fn update_settings(&self, mut settings: Settings, facts: &Facts) -> Result<Overview> {
        settings.normalize().map_err(invalid)?;
        let _guard = self.guard();
        self.require_installed()?;
        let mut state = self.load_state();
        state.model.settings = settings;
        self.commit(state, facts)
    }

    pub fn upsert_jail(&self, mut jail: JailConfig, facts: &Facts) -> Result<Overview> {
        jail.normalize().map_err(invalid)?;
        let known = model::known(&jail.name).expect("normalize checked the name");
        if known.needs_web && !facts.web_installed && jail.enabled {
            return Err(precondition(format!(
                "the {} jail reads the web server's logs; install the web server first",
                jail.name
            )));
        }
        let _guard = self.guard();
        self.require_installed()?;
        let mut state = self.load_state();
        match state.model.jails.iter_mut().find(|j| j.name == jail.name) {
            Some(existing) => *existing = jail,
            None => state.model.jails.push(jail),
        }
        self.commit(state, facts)
    }

    /// Applies the new model and saves it only if fail2ban accepted it.
    fn commit(&self, mut state: State, facts: &Facts) -> Result<Overview> {
        match self.apply_config(&state, facts) {
            Ok(warning) => {
                state.last_error = warning.unwrap_or_default();
                self.save_state(&state)?;
                self.forget_statuses();
                Ok(self.overview_locked(facts))
            }
            Err(err) => {
                let mut previous = self.load_state();
                previous.last_error = format!("{err:#}");
                let _ = self.save_state(&previous);
                Err(err)
            }
        }
    }

    /// Writes `asc.local`, has fail2ban check it and reloads; puts the
    /// previous file back when fail2ban refuses the new one.
    fn apply_config(&self, state: &State, facts: &Facts) -> Result<Option<String>> {
        let path = self.paths.config_file();
        let text = config::render(&state.model, facts);
        let previous = std::fs::read(&path).ok();
        write_atomic(&path, text.as_bytes(), 0o644).context("cannot write the fail2ban config")?;
        if let Err(err) = self.client.test_config() {
            match &previous {
                Some(old) => {
                    let _ = write_atomic(&path, old, 0o644);
                }
                None => {
                    let _ = std::fs::remove_file(&path);
                }
            }
            return Err(precondition(format!("{err:#}")));
        }
        let warning = if self.client.running() {
            self.client.reload()?;
            self.settle()?
        } else {
            self.client.start()?;
            None
        };
        self.forget_statuses();
        Ok(warning)
    }

    /// `fail2ban-client reload` can leave a jail without any ban action when
    /// the action it used is replaced (seen on a fresh install, where the
    /// stock config bans with `nftables` and ours with `nftables-multiport`):
    /// the jail runs and counts failures but bans nobody. A restart fixes it,
    /// so check the jails we configure and restart if one lost its actions.
    /// A warning means even the restart did not help.
    fn settle(&self) -> Result<Option<String>> {
        let broken = |this: &Self| -> Result<Vec<String>> {
            let mut out = Vec::new();
            for name in this.client.jails()? {
                if model::known(&name).is_some() && this.client.jail_actions(&name)?.is_empty() {
                    out.push(name);
                }
            }
            Ok(out)
        };
        if broken(self)?.is_empty() {
            return Ok(None);
        }
        tracing::info!("a fail2ban jail lost its ban action on reload; restarting fail2ban");
        self.client.restart()?;
        let still = broken(self)?;
        Ok((!still.is_empty()).then(|| {
            format!(
                "fail2ban runs the {} jail without a ban action, so it detects but does not ban; check `fail2ban-client get <jail> actions`",
                still.join(", ")
            )
        }))
    }

    // ── Bans ────────────────────────────────────────────────────────────────

    pub fn ban(&self, jail: &str, ip: &str) -> Result<()> {
        let ip = single_ip(ip)?;
        if model::known(jail).is_none() {
            return Err(invalid(format!("unknown jail '{jail}'")));
        }
        let _guard = self.guard();
        self.require_installed()?;
        if !self.client.jails()?.iter().any(|j| j == jail) {
            return Err(precondition(format!("the {jail} jail is not running")));
        }
        self.client.ban(jail, &ip)?;
        self.forget_statuses();
        Ok(())
    }

    /// Releases `ip`; an empty `jail` means every jail. `false` when it was
    /// not banned.
    pub fn unban(&self, jail: &str, ip: &str) -> Result<bool> {
        let ip = single_ip(ip)?;
        if !jail.is_empty() && model::known(jail).is_none() {
            return Err(invalid(format!("unknown jail '{jail}'")));
        }
        let _guard = self.guard();
        self.require_installed()?;
        let released = self.client.unban(jail, &ip)?;
        self.forget_statuses();
        Ok(released)
    }
}

impl Default for Fail2ban {
    fn default() -> Self {
        Self::new()
    }
}

/// One address (not a network): a ban is per address.
fn single_ip(value: &str) -> Result<String> {
    value
        .trim()
        .parse::<std::net::IpAddr>()
        .map(|ip| ip.to_string())
        .map_err(|_| invalid(format!("'{}' is not an IP address", value.trim())))
}

#[cfg(test)]
mod tests;
