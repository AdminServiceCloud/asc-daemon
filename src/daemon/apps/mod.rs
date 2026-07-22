//! App management core (DMN-002): storage, drivers, ownership, recovery.
//!
//! Ownership model: every app belongs to the Linux user who installed it.
//! A regular user sees and controls only their own apps; root (incl. sudo)
//! sees everyone's. The daemon API applies the same rule via request context.

pub mod disk;
pub mod docker;
pub mod driver;
pub mod meta;
pub mod ports;
pub mod process;
pub mod store;
pub mod systemd;

use anyhow::{Result, bail};
use tracing::{info, warn};

use crate::daemon::config::Config;
use crate::daemon::i18n::{Msg, tf};

pub use driver::{ResourceUsage, RuntimeState};
pub use meta::{AppMeta, DesiredState};
pub use store::AppStore;

/// Who is asking: determines the visible group of apps.
#[derive(Debug, Clone)]
pub struct UserContext {
    pub uid: u32,
    pub name: String,
    pub is_root: bool,
}

impl UserContext {
    /// Context of the calling user.
    ///
    /// Under sudo the effective uid is 0 (full visibility), while new apps
    /// are attributed to the invoking user via `SUDO_UID`/`SUDO_USER`.
    pub fn current() -> Self {
        // SAFETY: geteuid() has no preconditions and cannot fail.
        let euid = unsafe { libc::geteuid() };
        let is_root = euid == 0;
        let uid = std::env::var("SUDO_UID")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|_| is_root)
            .unwrap_or(euid);
        let name = std::env::var("SUDO_USER")
            .ok()
            .filter(|_| is_root)
            .or_else(|| std::env::var("USER").ok())
            .or_else(|| std::env::var("LOGNAME").ok())
            .unwrap_or_else(|| uid.to_string());
        Self { uid, name, is_root }
    }

    /// Context of a unix-socket peer, from its kernel-reported uid
    /// (SO_PEERCRED — unforgeable, unlike anything inside the request).
    ///
    /// `sudo_uid`/`sudo_user` are the client's *attribution hint* for `sudo
    /// asc ...` (mirroring [`UserContext::current`]'s `SUDO_UID`/`SUDO_USER`
    /// handling) and are honored only when the peer itself is root — a
    /// regular user cannot claim someone else's identity by sending headers.
    pub fn from_peer(peer_uid: u32, sudo_uid: Option<u32>, sudo_user: Option<&str>) -> Self {
        let is_root = peer_uid == 0;
        let uid = sudo_uid.filter(|_| is_root).unwrap_or(peer_uid);
        let name = sudo_user
            .filter(|_| is_root)
            .map(str::to_string)
            .or_else(|| username_for_uid(uid))
            .unwrap_or_else(|| uid.to_string());
        Self { uid, name, is_root }
    }
}

/// Login name for a uid from the user database, `None` when the uid has no
/// passwd entry (deleted user, container without the host's /etc/passwd).
fn username_for_uid(uid: u32) -> Option<String> {
    // _SC_GETPW_R_SIZE_MAX is a hint and may be -1; 4 KiB covers any sane
    // passwd line and getpwuid_r reports ERANGE if it somehow does not.
    let mut buf = vec![0i8; 4096];
    // SAFETY: zeroed passwd is a valid out-parameter for getpwuid_r.
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: all pointers reference live buffers of the stated sizes.
    let rc = unsafe {
        libc::getpwuid_r(
            uid,
            &mut pwd,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() {
        return None;
    }
    // SAFETY: on success pw_name points at a NUL-terminated string in buf.
    let name = unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) };
    Some(name.to_string_lossy().into_owned())
}

/// Outcome of start/stop, so the CLI can report idempotent calls honestly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Done,
    AlreadyInState,
}

/// One installed app with its observed runtime state.
pub struct AppStatus {
    pub meta: AppMeta,
    pub state: RuntimeState,
}

/// One app's resource consumption for `asc stats` (DMN-006).
pub struct AppStats {
    pub meta: AppMeta,
    /// CPU share since the previous sample; can exceed 100 on multi-core
    /// machines, like `docker stats`. `None` when the app is stopped or the
    /// runtime cannot report counters.
    pub cpu_percent: Option<f64>,
    /// Resident memory, bytes. `None` when the app is stopped.
    pub memory_bytes: Option<u64>,
    /// Everything under the app's directory, in bytes — the same figure
    /// `asc app disk` measures, computed here without the (comparatively
    /// expensive) image/volume breakdown so it stays cheap to refresh.
    pub disk_bytes: u64,
    /// `meta.quota.disk_bytes`, if the app has a disk quota set.
    pub quota_disk_bytes: Option<u64>,
}

/// CPU percentage from two cumulative readings over a wall-clock interval.
fn cpu_percent(first: &ResourceUsage, second: &ResourceUsage, elapsed_micros: u64) -> f64 {
    if elapsed_micros == 0 {
        return 0.0;
    }
    let delta = second.cpu_time_micros.saturating_sub(first.cpu_time_micros);
    delta as f64 / elapsed_micros as f64 * 100.0
}

pub struct AppManager {
    store: AppStore,
    /// The full config: drivers need `[docker]`, the settings refresh on
    /// start (DMN-017) resolves stack manifests through the registries.
    config: Config,
}

impl AppManager {
    pub fn new(config: &Config) -> Self {
        Self {
            store: AppStore::new(config.daemon.apps_dir.clone()),
            config: config.clone(),
        }
    }

    pub fn store(&self) -> &AppStore {
        &self.store
    }

    /// Apps visible to this user (root sees all), with runtime state.
    pub fn list(&self, ctx: &UserContext) -> Result<Vec<AppStatus>> {
        let mut result = Vec::new();
        for meta in self.store.list()? {
            if !ctx.is_root && meta.owner.uid != ctx.uid {
                continue;
            }
            let state = self.state_of(&meta);
            result.push(AppStatus { meta, state });
        }
        Ok(result)
    }

    /// Load an app the user is allowed to manage. `reference` is the app id
    /// or its custom name (DMN-024) — every command accepts both.
    ///
    /// A foreign app reports "not found" — same as a missing one — so users
    /// cannot probe which app ids exist on the server.
    pub fn get_authorized(&self, ctx: &UserContext, reference: &str) -> Result<AppMeta> {
        // The id first: ids are unique, custom names may not be. A reference
        // that is not even a valid id (spaces, uppercase) skips straight to
        // the name lookup.
        if meta::validate_id(reference).is_ok()
            && let Some(meta) = self.store.get(reference)?
            && (ctx.is_root || meta.owner.uid == ctx.uid)
        {
            return Ok(meta);
        }
        // Custom names are matched among visible apps only, so a name equal
        // to a foreign user's id still resolves to the caller's own app.
        let mut matches = self
            .store
            .list()?
            .into_iter()
            .filter(|m| ctx.is_root || m.owner.uid == ctx.uid)
            .filter(|m| m.custom_name.as_deref() == Some(reference));
        match (matches.next(), matches.next()) {
            (Some(meta), None) => Ok(meta),
            (Some(_), Some(_)) => bail!(tf(Msg::AppNameAmbiguous, reference)),
            _ => bail!(tf(Msg::AppNotFound, reference)),
        }
    }

    /// Observed state; errors (docker missing etc.) degrade to Stopped with a warning.
    fn state_of(&self, meta: &AppMeta) -> RuntimeState {
        let dir = match self.store.app_dir(&meta.id) {
            Ok(dir) => dir,
            Err(_) => return RuntimeState::Stopped,
        };
        match driver::for_runtime(&meta.runtime, &self.config.docker).state(meta, &dir) {
            Ok(state) => state,
            Err(err) => {
                warn!(app = %meta.id, error = %format!("{err:#}"), "cannot query app state");
                RuntimeState::Stopped
            }
        }
    }

    /// Resource counters of one app right now; `None` when stopped or the
    /// runtime cannot report them. Errors degrade to `None` with a warning —
    /// a broken Docker socket must not fail the whole stats table.
    fn usage_of(&self, meta: &AppMeta) -> Option<ResourceUsage> {
        let dir = self.store.app_dir(&meta.id).ok()?;
        match driver::for_runtime(&meta.runtime, &self.config.docker).usage(meta, &dir) {
            Ok(usage) => usage,
            Err(err) => {
                warn!(app = %meta.id, error = %format!("{err:#}"), "cannot query app usage");
                None
            }
        }
    }

    /// Resource consumption of the user's apps (root sees all), like
    /// `docker stats --no-stream`: two samples ~500 ms apart give the CPU
    /// percentage; memory comes from the second sample. Blocks for the
    /// sampling interval.
    pub fn stats(&self, ctx: &UserContext) -> Result<Vec<AppStats>> {
        let apps = self.list(ctx)?;
        let first: Vec<Option<ResourceUsage>> =
            apps.iter().map(|app| self.usage_of(&app.meta)).collect();
        let started = std::time::Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(500));
        let elapsed_micros = started.elapsed().as_micros() as u64;

        let mut result = Vec::with_capacity(apps.len());
        for (app, first) in apps.into_iter().zip(first) {
            let second = self.usage_of(&app.meta);
            let cpu = match (&first, &second) {
                (Some(a), Some(b)) => Some(cpu_percent(a, b, elapsed_micros)),
                _ => None,
            };
            let disk_bytes = self
                .store
                .app_dir(&app.meta.id)
                .map(|dir| disk::dir_size(&dir))
                .unwrap_or(0);
            let quota_disk_bytes = app.meta.quota.as_ref().and_then(|q| q.disk_bytes);
            result.push(AppStats {
                meta: app.meta,
                cpu_percent: cpu,
                memory_bytes: second.map(|u| u.memory_bytes),
                disk_bytes,
                quota_disk_bytes,
            });
        }
        Ok(result)
    }

    pub fn status(&self, ctx: &UserContext, id: &str) -> Result<AppStatus> {
        let meta = self.get_authorized(ctx, id)?;
        let state = self.state_of(&meta);
        Ok(AppStatus { meta, state })
    }

    pub fn start(&self, ctx: &UserContext, id: &str) -> Result<Outcome> {
        let mut meta = self.get_authorized(ctx, id)?;
        let dir = self.store.app_dir(&meta.id)?;
        let mut refreshed = false;
        let outcome = if self.state_of(&meta) == RuntimeState::Running {
            Outcome::AlreadyInState
        } else {
            // Changed settings (DMN-017/030) land here: a stopped container
            // whose configuration drifted from the settings is recreated.
            refreshed = crate::daemon::pkg::refresh::apply_settings(&self.config, &mut meta, &dir)?;
            driver::for_runtime(&meta.runtime, &self.config.docker).start(&meta, &dir)?;
            Outcome::Done
        };
        // Persist intent even when already running: survive the next reboot.
        if refreshed || meta.desired_state != DesiredState::Running {
            meta.desired_state = DesiredState::Running;
            self.store.save(&meta)?;
        }
        Ok(outcome)
    }

    pub fn stop(&self, ctx: &UserContext, id: &str) -> Result<Outcome> {
        let mut meta = self.get_authorized(ctx, id)?;
        let dir = self.store.app_dir(&meta.id)?;
        let outcome = if self.state_of(&meta) == RuntimeState::Stopped {
            Outcome::AlreadyInState
        } else {
            driver::for_runtime(&meta.runtime, &self.config.docker).stop(&meta, &dir)?;
            Outcome::Done
        };
        if meta.desired_state != DesiredState::Stopped {
            meta.desired_state = DesiredState::Stopped;
            self.store.save(&meta)?;
        }
        Ok(outcome)
    }

    pub fn restart(&self, ctx: &UserContext, id: &str) -> Result<()> {
        let mut meta = self.get_authorized(ctx, id)?;
        let dir = self.store.app_dir(&meta.id)?;
        // Docker restart runs as stop + start so changed settings (DMN-017/
        // 030) apply through the recreate in `apply_settings` — restart is
        // the documented way to pick up new setting values.
        let mut refreshed = false;
        if matches!(meta.runtime, meta::Runtime::Docker { .. }) {
            driver::for_runtime(&meta.runtime, &self.config.docker).stop(&meta, &dir)?;
            refreshed = crate::daemon::pkg::refresh::apply_settings(&self.config, &mut meta, &dir)?;
            driver::for_runtime(&meta.runtime, &self.config.docker).start(&meta, &dir)?;
        } else {
            driver::for_runtime(&meta.runtime, &self.config.docker).restart(&meta, &dir)?;
        }
        if refreshed || meta.desired_state != DesiredState::Running {
            meta.desired_state = DesiredState::Running;
            self.store.save(&meta)?;
        }
        Ok(())
    }

    pub fn logs(&self, ctx: &UserContext, id: &str, tail: usize) -> Result<String> {
        let meta = self.get_authorized(ctx, id)?;
        let dir = self.store.app_dir(&meta.id)?;
        driver::for_runtime(&meta.runtime, &self.config.docker).logs(&meta, &dir, tail)
    }

    /// Remove the app: release runtime resources, then delete its directory.
    pub fn remove(&self, ctx: &UserContext, id: &str) -> Result<()> {
        let meta = self.get_authorized(ctx, id)?;
        let dir = self.store.app_dir(&meta.id)?;
        driver::for_runtime(&meta.runtime, &self.config.docker).remove(&meta, &dir)?;
        self.store.remove(&meta.id)
    }

    /// Startup recovery: bring every app back to its desired state
    /// (e.g. after a server reboot). Failures are logged, not fatal —
    /// one broken app must not block the daemon or the other apps.
    pub fn reconcile(&self) -> Result<()> {
        for mut meta in self.store.list()? {
            if meta.desired_state != DesiredState::Running {
                continue;
            }
            if self.state_of(&meta) == RuntimeState::Running {
                continue;
            }
            let dir = match self.store.app_dir(&meta.id) {
                Ok(dir) => dir,
                Err(err) => {
                    warn!(app = %meta.id, error = %err, "reconcile: bad app dir");
                    continue;
                }
            };
            // Settings changed while the daemon was down still apply.
            match crate::daemon::pkg::refresh::apply_settings(&self.config, &mut meta, &dir) {
                Ok(true) => {
                    if let Err(err) = self.store.save(&meta) {
                        warn!(app = %meta.id, error = %format!("{err:#}"), "reconcile: cannot save meta");
                    }
                }
                Ok(false) => {}
                Err(err) => {
                    warn!(app = %meta.id, error = %format!("{err:#}"), "reconcile: cannot apply settings");
                    continue;
                }
            }
            match driver::for_runtime(&meta.runtime, &self.config.docker).start(&meta, &dir) {
                Ok(()) => info!(app = %meta.id, "reconcile: app started"),
                Err(err) => {
                    warn!(app = %meta.id, error = %format!("{err:#}"), "reconcile: cannot start app");
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::meta::{Owner, Runtime};
    use super::*;

    fn manager(root: &std::path::Path) -> AppManager {
        let mut config = Config::default();
        config.daemon.apps_dir = root.to_path_buf();
        AppManager::new(&config)
    }

    fn install(mgr: &AppManager, id: &str, uid: u32) {
        install_named(mgr, id, uid, None);
    }

    fn install_named(mgr: &AppManager, id: &str, uid: u32, custom_name: Option<&str>) {
        mgr.store()
            .save(&AppMeta {
                id: id.into(),
                uuid: None,
                name: id.into(),
                custom_name: custom_name.map(Into::into),
                owner: Owner {
                    uid,
                    name: format!("user{uid}"),
                },
                version: None,
                source: None,
                package: None,
                desired_state: DesiredState::Stopped,
                quota: None,
                runtime: Runtime::Process {
                    command: "true".into(),
                    args: vec![],
                },
            })
            .unwrap();
    }

    fn user(uid: u32) -> UserContext {
        UserContext {
            uid,
            name: format!("user{uid}"),
            is_root: false,
        }
    }

    fn root() -> UserContext {
        UserContext {
            uid: 0,
            name: "root".into(),
            is_root: true,
        }
    }

    #[test]
    fn users_see_only_their_apps_root_sees_all() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = manager(dir.path());
        install(&mgr, "alice-app", 1000);
        install(&mgr, "bob-app", 1001);

        let alice: Vec<_> = mgr
            .list(&user(1000))
            .unwrap()
            .into_iter()
            .map(|s| s.meta.id)
            .collect();
        assert_eq!(alice, ["alice-app"]);
        assert_eq!(mgr.list(&root()).unwrap().len(), 2);
    }

    #[test]
    fn cpu_percent_is_a_delta_over_wall_clock() {
        let a = ResourceUsage {
            cpu_time_micros: 1_000_000,
            memory_bytes: 0,
        };
        let b = ResourceUsage {
            cpu_time_micros: 1_250_000,
            memory_bytes: 0,
        };
        // 250ms of CPU over 500ms of wall clock = 50%.
        assert!((cpu_percent(&a, &b, 500_000) - 50.0).abs() < 1e-9);
        // Counter went backwards (restart) → 0, not a negative percentage.
        assert_eq!(cpu_percent(&b, &a, 500_000), 0.0);
        assert_eq!(cpu_percent(&a, &b, 0), 0.0);
    }

    #[test]
    fn stats_reports_running_process_and_skips_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = manager(dir.path());
        install(&mgr, "stopped-app", 1000);
        mgr.store()
            .save(&AppMeta {
                id: "sleeper".into(),
                uuid: None,
                name: "sleeper".into(),
                custom_name: None,
                owner: Owner {
                    uid: 1000,
                    name: "user1000".into(),
                },
                version: None,
                source: None,
                package: None,
                desired_state: DesiredState::Stopped,
                quota: None,
                runtime: Runtime::Process {
                    command: "sleep".into(),
                    args: vec!["30".into()],
                },
            })
            .unwrap();
        mgr.start(&user(1000), "sleeper").unwrap();

        let stats = mgr.stats(&user(1000)).unwrap();
        let sleeper = stats.iter().find(|s| s.meta.id == "sleeper").unwrap();
        assert!(sleeper.memory_bytes.unwrap() > 0);
        assert!(sleeper.cpu_percent.unwrap() >= 0.0);
        let stopped = stats.iter().find(|s| s.meta.id == "stopped-app").unwrap();
        assert!(stopped.memory_bytes.is_none());
        assert!(stopped.cpu_percent.is_none());

        mgr.stop(&user(1000), "sleeper").unwrap();
    }

    #[test]
    fn foreign_app_reads_as_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = manager(dir.path());
        install(&mgr, "alice-app", 1000);

        assert!(mgr.get_authorized(&user(1001), "alice-app").is_err());
        assert!(mgr.get_authorized(&user(1000), "alice-app").is_ok());
        assert!(mgr.get_authorized(&root(), "alice-app").is_ok());
    }

    #[test]
    fn custom_name_resolves_like_the_id() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = manager(dir.path());
        install_named(&mgr, "cs2-server", 1000, Some("My CS2 Server"));

        // Both the id and the custom name reach the same app.
        assert_eq!(
            mgr.get_authorized(&user(1000), "cs2-server").unwrap().id,
            "cs2-server"
        );
        assert_eq!(
            mgr.get_authorized(&user(1000), "My CS2 Server").unwrap().id,
            "cs2-server"
        );
        // Foreign users cannot resolve the name either.
        assert!(mgr.get_authorized(&user(1001), "My CS2 Server").is_err());
        assert!(mgr.get_authorized(&root(), "My CS2 Server").is_ok());
    }

    #[test]
    fn id_wins_over_a_colliding_custom_name() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = manager(dir.path());
        install(&mgr, "nginx", 1000);
        // A second app whose custom name shadows the first app's id.
        install_named(&mgr, "other", 1000, Some("nginx"));

        assert_eq!(
            mgr.get_authorized(&user(1000), "nginx").unwrap().id,
            "nginx"
        );
        assert_eq!(
            mgr.get_authorized(&user(1000), "other").unwrap().id,
            "other"
        );
    }

    #[test]
    fn ambiguous_custom_name_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = manager(dir.path());
        install_named(&mgr, "app-a", 1000, Some("Server"));
        install_named(&mgr, "app-b", 1000, Some("Server"));

        assert!(mgr.get_authorized(&user(1000), "Server").is_err());
        assert!(mgr.get_authorized(&user(1000), "app-a").is_ok());
    }

    /// SO_PEERCRED contexts: a non-root peer is exactly itself — the sudo
    /// attribution hint must be ignored (headers are client-controlled);
    /// a root peer keeps full visibility while attributing new apps to the
    /// invoking sudo user, mirroring `UserContext::current`.
    #[test]
    fn peer_context_honors_sudo_hint_only_for_root() {
        let plain = UserContext::from_peer(1000, None, None);
        assert_eq!(plain.uid, 1000);
        assert!(!plain.is_root);

        let spoofed = UserContext::from_peer(1000, Some(0), Some("root"));
        assert_eq!(spoofed.uid, 1000, "non-root peer must not escalate");
        assert!(!spoofed.is_root);
        assert_ne!(spoofed.name, "root");

        let sudo = UserContext::from_peer(0, Some(1000), Some("alice"));
        assert_eq!(sudo.uid, 1000);
        assert_eq!(sudo.name, "alice");
        assert!(sudo.is_root, "sudo keeps full visibility");

        let root = UserContext::from_peer(0, None, None);
        assert_eq!(root.uid, 0);
        assert!(root.is_root);
    }
}
