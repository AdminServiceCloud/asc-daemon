//! Compose driver: manages a `docker compose` project. Whatever needs the
//! project model — `up`, `stop`, `kill`, `down`, the multiplexed logs — goes
//! through the CLI plugin (see [`crate::daemon::compose`]); reading the
//! project's state and usage only needs its containers, which bollard finds
//! by the `com.docker.compose.project` label compose puts on each of them.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use super::driver::{AppDriver, ResourceUsage, RuntimeState};
use super::meta::{AppMeta, Runtime};
use crate::daemon::compose::{self, ComposeProject};
use crate::daemon::config::DockerConfig;
use crate::daemon::docker;

/// The daemon's own compose override file, inside the app's `.asc/` state
/// directory — outside `repository/`, so no git operation ever touches it.
/// It carries the per-service settings ([`super::compose_services`]) and is
/// given to the plugin last, so it wins over the package's files.
pub const OVERRIDE_FILE: &str = "compose.override.yaml";

/// Where override entries of services that vanished from the compose files
/// go after an upgrade, instead of being dropped silently.
pub const ORPHANED_OVERRIDE_FILE: &str = "compose.override.orphaned.yaml";

/// `<app_dir>/.asc/compose.override.yaml`.
pub fn override_path(app_dir: &Path) -> PathBuf {
    super::layout::state_dir(app_dir).join(OVERRIDE_FILE)
}

/// The one place a compose app's plugin invocation is assembled: its
/// project name, its files in order and the directory they resolve from —
/// plus the daemon's override file as the last `-f` when it exists. The
/// override is never recorded in `meta.files`.
pub fn project_of(meta: &AppMeta, app_dir: &Path) -> Result<ComposeProject> {
    let base = base_project_of(meta, app_dir)?;
    let override_file = override_path(app_dir);
    Ok(if override_file.is_file() {
        base.with_file(&override_file)
    } else {
        base
    })
}

/// [`project_of`] without the daemon's override file: the project exactly as
/// the package's own files define it — what tells which services exist.
pub fn base_project_of(meta: &AppMeta, app_dir: &Path) -> Result<ComposeProject> {
    match &meta.runtime {
        Runtime::Compose {
            project,
            files,
            working_dir,
            ..
        } => Ok(ComposeProject::new(
            project.clone(),
            app_dir.join(working_dir),
            files.clone(),
        )),
        other => bail!("app '{}' is not a compose app ({})", meta.id, other.kind()),
    }
}

/// The compose project name of a compose app.
fn project_name(meta: &AppMeta) -> Result<&str> {
    match &meta.runtime {
        Runtime::Compose { project, .. } => Ok(project),
        other => bail!("app '{}' is not a compose app ({})", meta.id, other.kind()),
    }
}

pub struct ComposeDriver {
    cfg: DockerConfig,
}

impl ComposeDriver {
    pub fn new(cfg: DockerConfig) -> Self {
        Self { cfg }
    }
}

impl AppDriver for ComposeDriver {
    fn start(&self, meta: &AppMeta, dir: &Path) -> Result<()> {
        compose::up(&self.cfg, &project_of(meta, dir)?)
    }

    fn stop(&self, meta: &AppMeta, dir: &Path) -> Result<()> {
        compose::stop(&self.cfg, &project_of(meta, dir)?)
    }

    fn kill(&self, meta: &AppMeta, dir: &Path) -> Result<()> {
        compose::kill(&self.cfg, &project_of(meta, dir)?)
    }

    /// Running while any container of the project runs — read from the
    /// Engine by label, no plugin process involved.
    fn state(&self, meta: &AppMeta, _dir: &Path) -> Result<RuntimeState> {
        let containers = docker::list_project_containers(&self.cfg, project_name(meta)?)?;
        Ok(state_of(&containers))
    }

    /// Summed across every running container of the project. Disk/network
    /// counters and the start time are reported only when every running
    /// container itself reports them — a compose app has no single "the"
    /// container to attribute a partial reading to.
    fn usage(&self, meta: &AppMeta, _dir: &Path) -> Result<Option<ResourceUsage>> {
        let containers = docker::list_project_containers(&self.cfg, project_name(meta)?)?;
        let running: Vec<&str> = containers
            .iter()
            .filter(|c| c.state == "running")
            .map(|c| c.id.as_str())
            .collect();
        if running.is_empty() {
            return Ok(None);
        }
        let mut total = ResourceUsage {
            cpu_time_micros: 0,
            memory_bytes: 0,
            disk_read_bytes: Some(0),
            disk_write_bytes: Some(0),
            net_rx_bytes: Some(0),
            net_tx_bytes: Some(0),
            started_at: None,
        };
        for id in running {
            let Some(usage) = docker::stats_usage(&self.cfg, id)? else {
                continue;
            };
            total.cpu_time_micros += usage.cpu_time_micros;
            total.memory_bytes += usage.memory_bytes;
            total.disk_read_bytes = add_or_none(total.disk_read_bytes, usage.disk_read_bytes);
            total.disk_write_bytes = add_or_none(total.disk_write_bytes, usage.disk_write_bytes);
            total.net_rx_bytes = add_or_none(total.net_rx_bytes, usage.net_rx_bytes);
            total.net_tx_bytes = add_or_none(total.net_tx_bytes, usage.net_tx_bytes);
            if let Some(started) = docker::started_at(&self.cfg, id)? {
                total.started_at = Some(total.started_at.map_or(started, |t: i64| t.min(started)));
            }
        }
        Ok(Some(total))
    }

    fn logs(&self, meta: &AppMeta, dir: &Path, tail: usize, timestamps: bool) -> Result<String> {
        compose::logs(&self.cfg, &project_of(meta, dir)?, tail, timestamps)
    }

    /// `down`, not `stop` — removing the app tears the project down
    /// entirely, same as removing any other runtime's containers/units.
    fn remove(&self, meta: &AppMeta, dir: &Path) -> Result<()> {
        compose::down(&self.cfg, &project_of(meta, dir)?)
    }
}

/// A project is running while any of its containers runs.
pub(crate) fn state_of(containers: &[docker::ContainerInfo]) -> RuntimeState {
    if containers.iter().any(|c| c.state == "running") {
        RuntimeState::Running
    } else {
        RuntimeState::Stopped
    }
}

fn add_or_none(total: Option<u64>, value: Option<u64>) -> Option<u64> {
    match (total, value) {
        (Some(total), Some(value)) => Some(total + value),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::apps::meta::{DesiredState, Owner};

    fn meta() -> AppMeta {
        AppMeta {
            id: "demo".into(),
            uuid: None,
            name: "demo".into(),
            custom_name: None,
            owner: Owner {
                uid: 0,
                name: "root".into(),
            },
            version: None,
            source: None,
            branch: None,
            repo_path: None,
            package: None,
            install_method: None,
            desired_state: DesiredState::Stopped,
            quota: None,
            runtime: Runtime::Compose {
                project: "asc-demo".into(),
                files: vec!["docker-compose.yml".into()],
                working_dir: "repository".into(),
                services: Vec::new(),
            },
        }
    }

    #[test]
    fn the_override_is_appended_only_when_it_exists() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_of(&meta(), dir.path()).unwrap();
        assert_eq!(project.project, "asc-demo");
        assert_eq!(project.working_dir, dir.path().join("repository"));
        assert_eq!(project.files, ["docker-compose.yml"]);

        std::fs::create_dir_all(dir.path().join(".asc")).unwrap();
        std::fs::write(override_path(dir.path()), "services: {}\n").unwrap();
        let project = project_of(&meta(), dir.path()).unwrap();
        assert_eq!(
            project.files,
            [
                "docker-compose.yml".to_string(),
                override_path(dir.path()).to_string_lossy().into_owned()
            ]
        );
        // The base project never sees it: it is what defines the services.
        assert_eq!(
            base_project_of(&meta(), dir.path()).unwrap().files,
            ["docker-compose.yml"]
        );
        // And the override stays outside the repository git manages.
        assert!(!override_path(dir.path()).starts_with(dir.path().join("repository")));
    }

    #[test]
    fn other_runtimes_have_no_project() {
        let mut meta = meta();
        meta.runtime = Runtime::Process {
            command: "true".into(),
            args: vec![],
        };
        assert!(project_of(&meta, Path::new("/x")).is_err());
    }
}
