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
        Ok(docker::stats_usage(&self.cfg, container_name(meta)?)?.map(
            |(cpu_time_micros, memory_bytes)| ResourceUsage {
                cpu_time_micros,
                memory_bytes,
            },
        ))
    }

    fn logs(&self, meta: &AppMeta, _dir: &Path, tail: usize) -> Result<String> {
        docker::logs_tail(&self.cfg, container_name(meta)?, tail)
    }

    fn remove(&self, meta: &AppMeta, _dir: &Path) -> Result<()> {
        docker::remove(&self.cfg, container_name(meta)?)
    }
}
