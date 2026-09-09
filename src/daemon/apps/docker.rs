//! Docker driver: manages an existing container through the Docker Engine
//! API (see [`crate::daemon::docker`]), addressed by the configured socket.

use std::path::Path;

use anyhow::{Result, bail};

use super::driver::{AppDriver, ResourceUsage, RuntimeState};
use super::meta::{AppMeta, Runtime};
use crate::daemon::config::DockerConfig;
use crate::daemon::docker;

pub struct DockerDriver {
    cfg: DockerConfig,
}

impl DockerDriver {
    pub fn new(cfg: DockerConfig) -> Self {
        Self { cfg }
    }
}

fn container_name(meta: &AppMeta) -> Result<&str> {
    match &meta.runtime {
        Runtime::Docker { container, .. } => Ok(container),
        other => bail!("app '{}' is not a docker app ({})", meta.id, other.kind()),
    }
}

impl AppDriver for DockerDriver {
    fn start(&self, meta: &AppMeta, _dir: &Path) -> Result<()> {
        docker::start(&self.cfg, container_name(meta)?)
    }

    fn stop(&self, meta: &AppMeta, _dir: &Path) -> Result<()> {
        docker::stop(&self.cfg, container_name(meta)?)
    }

    fn restart(&self, meta: &AppMeta, _dir: &Path) -> Result<()> {
        docker::restart(&self.cfg, container_name(meta)?)
    }

    fn state(&self, meta: &AppMeta, _dir: &Path) -> Result<RuntimeState> {
        if docker::running(&self.cfg, container_name(meta)?)? {
            Ok(RuntimeState::Running)
        } else {
            Ok(RuntimeState::Stopped)
        }
    }

    fn usage(&self, meta: &AppMeta, _dir: &Path) -> Result<Option<ResourceUsage>> {
        let name = container_name(meta)?;
        let Some(u) = docker::stats_usage(&self.cfg, name)? else {
            return Ok(None);
        };
        let started_at = docker::started_at(&self.cfg, name)?;
        Ok(Some(ResourceUsage {
            cpu_time_micros: u.cpu_time_micros,
            memory_bytes: u.memory_bytes,
            disk_read_bytes: u.disk_read_bytes,
            disk_write_bytes: u.disk_write_bytes,
            net_rx_bytes: u.net_rx_bytes,
            net_tx_bytes: u.net_tx_bytes,
            started_at,
        }))
    }

    fn logs(&self, meta: &AppMeta, _dir: &Path, tail: usize, timestamps: bool) -> Result<String> {
        docker::logs_tail(&self.cfg, container_name(meta)?, tail, timestamps)
    }

    fn remove(&self, meta: &AppMeta, _dir: &Path) -> Result<()> {
        docker::remove(&self.cfg, container_name(meta)?)
    }
}
