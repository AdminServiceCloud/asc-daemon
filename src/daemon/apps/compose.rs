//! Compose driver: manages a `docker compose` project (DMN-108) through the
//! CLI plugin (see [`crate::daemon::compose`]), never through bollard — a
//! compose project is exactly what this daemon deliberately does not model
//! on top of the Engine API itself.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use super::driver::{AppDriver, ResourceUsage, RuntimeState};
use super::meta::{AppMeta, Runtime};
use crate::daemon::compose;
use crate::daemon::config::DockerConfig;
use crate::daemon::docker;

pub struct ComposeDriver {
    cfg: DockerConfig,
}

impl ComposeDriver {
    pub fn new(cfg: DockerConfig) -> Self {
        Self { cfg }
    }
}

/// `(project, files, working_dir)` of a compose app, `working_dir` already
/// resolved to an absolute path under the app directory.
fn fields<'a>(meta: &'a AppMeta, dir: &Path) -> Result<(&'a str, &'a [String], PathBuf)> {
    match &meta.runtime {
        Runtime::Compose {
            project,
            files,
            working_dir,
        } => Ok((project, files, dir.join(working_dir))),
        other => bail!("app '{}' is not a compose app ({})", meta.id, other.kind()),
    }
}

impl AppDriver for ComposeDriver {
    fn start(&self, meta: &AppMeta, dir: &Path) -> Result<()> {
        let (project, files, working_dir) = fields(meta, dir)?;
        compose::up(&self.cfg, project, &working_dir, files)
    }

    fn stop(&self, meta: &AppMeta, dir: &Path) -> Result<()> {
        let (project, files, working_dir) = fields(meta, dir)?;
        compose::stop(&self.cfg, project, &working_dir, files)
    }

    fn state(&self, meta: &AppMeta, dir: &Path) -> Result<RuntimeState> {
        let (project, files, working_dir) = fields(meta, dir)?;
        let containers = compose::ps(&self.cfg, project, &working_dir, files)?;
        Ok(if containers.iter().any(|c| c.running) {
            RuntimeState::Running
        } else {
            RuntimeState::Stopped
        })
    }

    /// Summed across every running container of the project. Disk/network
    /// counters and the start time are reported only when every running
    /// container itself reports them — a compose app has no single "the"
    /// container to attribute a partial reading to.
    fn usage(&self, meta: &AppMeta, dir: &Path) -> Result<Option<ResourceUsage>> {
        let (project, files, working_dir) = fields(meta, dir)?;
        let containers = compose::ps(&self.cfg, project, &working_dir, files)?;
        let running: Vec<&str> = containers
            .iter()
            .filter(|c| c.running)
            .map(|c| c.name.as_str())
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
        for name in running {
            let Some(usage) = docker::stats_usage(&self.cfg, name)? else {
                continue;
            };
            total.cpu_time_micros += usage.cpu_time_micros;
            total.memory_bytes += usage.memory_bytes;
            total.disk_read_bytes = add_or_none(total.disk_read_bytes, usage.disk_read_bytes);
            total.disk_write_bytes = add_or_none(total.disk_write_bytes, usage.disk_write_bytes);
            total.net_rx_bytes = add_or_none(total.net_rx_bytes, usage.net_rx_bytes);
            total.net_tx_bytes = add_or_none(total.net_tx_bytes, usage.net_tx_bytes);
            if let Some(started) = docker::started_at(&self.cfg, name)? {
                total.started_at = Some(total.started_at.map_or(started, |t: i64| t.min(started)));
            }
        }
        Ok(Some(total))
    }

    fn logs(&self, meta: &AppMeta, dir: &Path, tail: usize, timestamps: bool) -> Result<String> {
        let (project, files, working_dir) = fields(meta, dir)?;
        compose::logs(&self.cfg, project, &working_dir, files, tail, timestamps)
    }

    /// `down`, not `stop` — removing the app tears the project down
    /// entirely, same as removing any other runtime's containers/units.
    fn remove(&self, meta: &AppMeta, dir: &Path) -> Result<()> {
        let (project, files, working_dir) = fields(meta, dir)?;
        compose::down(&self.cfg, project, &working_dir, files)
    }
}

fn add_or_none(total: Option<u64>, value: Option<u64>) -> Option<u64> {
    match (total, value) {
        (Some(total), Some(value)) => Some(total + value),
        _ => None,
    }
}
