//! [`AppDriver`] — one lifecycle interface over Docker containers, systemd
//! units and plain processes. Provisioning (creating the container/unit from
//! a package manifest) belongs to the package manager (DMN-003), not here.

use std::path::Path;

use anyhow::Result;

use super::docker::DockerDriver;
use super::meta::{AppMeta, Runtime};
use super::process::ProcessDriver;
use super::systemd::SystemdAppDriver;
use crate::daemon::config::DockerConfig;

/// Observed (actual) state of an app's runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeState {
    Running,
    Stopped,
}

/// Point-in-time resource counters of a running app (DMN-006).
///
/// CPU time is cumulative, so a usage percentage is a delta between two
/// readings — the caller samples twice (see `AppManager::stats`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceUsage {
    /// Total CPU time consumed since the app started, microseconds.
    pub cpu_time_micros: u64,
    /// Resident memory, bytes.
    pub memory_bytes: u64,
    /// Bytes read from block devices since the app started. `None` when the
    /// runtime cannot report it (e.g. cgroup v1 host).
    pub disk_read_bytes: Option<u64>,
    /// Bytes written to block devices since the app started. `None` when the
    /// runtime cannot report it.
    pub disk_write_bytes: Option<u64>,
    /// Bytes received over the network since the app started. Only Docker
    /// apps have their own network namespace to measure — `None` for
    /// systemd/process apps, which share the host's.
    pub net_rx_bytes: Option<u64>,
    /// Bytes sent over the network since the app started; see `net_rx_bytes`.
    pub net_tx_bytes: Option<u64>,
}

/// Lifecycle operations every runtime kind must support.
pub trait AppDriver {
    fn start(&self, meta: &AppMeta, dir: &Path) -> Result<()>;
    fn stop(&self, meta: &AppMeta, dir: &Path) -> Result<()>;

    /// Restart; drivers with a native restart (systemd) override this.
    fn restart(&self, meta: &AppMeta, dir: &Path) -> Result<()> {
        self.stop(meta, dir)?;
        self.start(meta, dir)
    }

    fn state(&self, meta: &AppMeta, dir: &Path) -> Result<RuntimeState>;

    /// Resource counters of the running app; `None` when it is stopped or
    /// the runtime cannot report them (e.g. cgroup v1 host).
    fn usage(&self, meta: &AppMeta, dir: &Path) -> Result<Option<ResourceUsage>>;

    /// Last `tail` lines of the app's logs.
    fn logs(&self, meta: &AppMeta, dir: &Path, tail: usize) -> Result<String>;

    /// Release runtime resources (container, unit, process). Files under the
    /// app directory are removed by the manager afterwards.
    fn remove(&self, meta: &AppMeta, dir: &Path) -> Result<()>;
}

/// Pick the driver matching the app's runtime. The Docker driver needs the
/// Engine socket config; other runtimes ignore it.
pub fn for_runtime(runtime: &Runtime, docker: &DockerConfig) -> Box<dyn AppDriver> {
    match runtime {
        Runtime::Docker { .. } => Box::new(DockerDriver::new(docker.clone())),
        Runtime::Systemd { .. } => Box::new(SystemdAppDriver),
        Runtime::Process { .. } => Box::new(ProcessDriver),
    }
}

/// Last `n` lines of a text buffer (used by file- and CLI-based log sources).
pub fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let skip = lines.len().saturating_sub(n);
    lines[skip..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_returns_last_lines() {
        assert_eq!(tail_lines("a\nb\nc\n", 2), "b\nc");
        assert_eq!(tail_lines("a", 5), "a");
        assert_eq!(tail_lines("", 5), "");
    }
}
