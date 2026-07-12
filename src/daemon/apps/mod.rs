//! App management core (DMN-002): storage, drivers, ownership, recovery.
//!
//! Ownership model: every app belongs to the Linux user who installed it.
//! A regular user sees and controls only their own apps; root (incl. sudo)
//! sees everyone's. The daemon API applies the same rule via request context.

pub mod docker;
pub mod driver;
pub mod meta;
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
    docker: crate::daemon::config::DockerConfig,
}

impl AppManager {
    pub fn new(config: &Config) -> Self {
        Self {
            store: AppStore::new(config.daemon.apps_dir.clone()),
            docker: config.docker.clone(),
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

    /// Load an app the user is allowed to manage.
    ///
    /// A foreign app reports "not found" — same as a missing one — so users
    /// cannot probe which app ids exist on the server.
    pub fn get_authorized(&self, ctx: &UserContext, id: &str) -> Result<AppMeta> {
        match self.store.get(id)? {
            Some(meta) if ctx.is_root || meta.owner.uid == ctx.uid => Ok(meta),
            _ => bail!(tf(Msg::AppNotFound, id)),
        }
    }

    /// Observed state; errors (docker missing etc.) degrade to Stopped with a warning.
    fn state_of(&self, meta: &AppMeta) -> RuntimeState {
        let dir = match self.store.app_dir(&meta.id) {
            Ok(dir) => dir,
            Err(_) => return RuntimeState::Stopped,
        };
        match driver::for_runtime(&meta.runtime, &self.docker).state(meta, &dir) {
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
        match driver::for_runtime(&meta.runtime, &self.docker).usage(meta, &dir) {
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
            result.push(AppStats {
                meta: app.meta,
                cpu_percent: cpu,
                memory_bytes: second.map(|u| u.memory_bytes),
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
        let dir = self.store.app_dir(id)?;
        let outcome = if self.state_of(&meta) == RuntimeState::Running {
            Outcome::AlreadyInState
        } else {
            driver::for_runtime(&meta.runtime, &self.docker).start(&meta, &dir)?;
            Outcome::Done
        };
        // Persist intent even when already running: survive the next reboot.
        if meta.desired_state != DesiredState::Running {
            meta.desired_state = DesiredState::Running;
            self.store.save(&meta)?;
        }
        Ok(outcome)
    }

    pub fn stop(&self, ctx: &UserContext, id: &str) -> Result<Outcome> {
        let mut meta = self.get_authorized(ctx, id)?;
        let dir = self.store.app_dir(id)?;
        let outcome = if self.state_of(&meta) == RuntimeState::Stopped {
            Outcome::AlreadyInState
        } else {
            driver::for_runtime(&meta.runtime, &self.docker).stop(&meta, &dir)?;
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
        let dir = self.store.app_dir(id)?;
        driver::for_runtime(&meta.runtime, &self.docker).restart(&meta, &dir)?;
        if meta.desired_state != DesiredState::Running {
            meta.desired_state = DesiredState::Running;
            self.store.save(&meta)?;
        }
        Ok(())
    }

    pub fn logs(&self, ctx: &UserContext, id: &str, tail: usize) -> Result<String> {
        let meta = self.get_authorized(ctx, id)?;
        let dir = self.store.app_dir(id)?;
        driver::for_runtime(&meta.runtime, &self.docker).logs(&meta, &dir, tail)
    }

    /// Remove the app: release runtime resources, then delete its directory.
    pub fn remove(&self, ctx: &UserContext, id: &str) -> Result<()> {
        let meta = self.get_authorized(ctx, id)?;
        let dir = self.store.app_dir(id)?;
        driver::for_runtime(&meta.runtime, &self.docker).remove(&meta, &dir)?;
        self.store.remove(id)
    }

    /// Startup recovery: bring every app back to its desired state
    /// (e.g. after a server reboot). Failures are logged, not fatal —
    /// one broken app must not block the daemon or the other apps.
    pub fn reconcile(&self) -> Result<()> {
        for meta in self.store.list()? {
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
            match driver::for_runtime(&meta.runtime, &self.docker).start(&meta, &dir) {
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
        mgr.store()
            .save(&AppMeta {
                id: id.into(),
                name: id.into(),
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
                name: "sleeper".into(),
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
}
