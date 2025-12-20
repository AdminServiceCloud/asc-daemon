//! systemd driver for native apps: one unit per app (`asc-app-<id>.service`).
//!
//! The unit file itself is created by the package manager at install time
//! (DMN-003); this driver only drives its lifecycle.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use super::driver::{AppDriver, ResourceUsage, RuntimeState};
use super::meta::{AppMeta, Runtime};
use crate::daemon::service::systemd::systemctl;

/// cgroup v2 directory of a system service unit.
fn cgroup_dir(unit: &str) -> String {
    format!("/sys/fs/cgroup/system.slice/{unit}")
}

/// `usage_usec` line of a cgroup v2 `cpu.stat` file.
fn parse_cpu_stat_usage_usec(raw: &str) -> Option<u64> {
    raw.lines()
        .find_map(|l| l.strip_prefix("usage_usec "))?
        .trim()
        .parse()
        .ok()
}

/// Read the unit's resource counters from its cgroup; `None` when the unit
/// is not running or the host has no unified cgroup hierarchy (v1).
fn cgroup_usage(unit: &str) -> Option<ResourceUsage> {
    let dir = cgroup_dir(unit);
    let cpu_stat = std::fs::read_to_string(format!("{dir}/cpu.stat")).ok()?;
    let memory = std::fs::read_to_string(format!("{dir}/memory.current")).ok()?;
    Some(ResourceUsage {
        cpu_time_micros: parse_cpu_stat_usage_usec(&cpu_stat)?,
        memory_bytes: memory.trim().parse().ok()?,
    })
}

/// systemd unit name for an app id.
pub fn unit_name(id: &str) -> String {
    format!("asc-app-{id}.service")
}

pub struct SystemdAppDriver;

fn unit(meta: &AppMeta) -> Result<&str> {
    match &meta.runtime {
        Runtime::Systemd { unit } => Ok(unit),
        other => bail!("app '{}' is not a systemd app ({})", meta.id, other.kind()),
    }
}

impl AppDriver for SystemdAppDriver {
    fn start(&self, meta: &AppMeta, _dir: &Path) -> Result<()> {
        systemctl(&["start", unit(meta)?])
    }

    fn stop(&self, meta: &AppMeta, _dir: &Path) -> Result<()> {
        systemctl(&["stop", unit(meta)?])
    }

    fn restart(&self, meta: &AppMeta, _dir: &Path) -> Result<()> {
        systemctl(&["restart", unit(meta)?])
    }

    fn state(&self, meta: &AppMeta, _dir: &Path) -> Result<RuntimeState> {
        let out = Command::new("systemctl")
            .args(["is-active", unit(meta)?])
            .output()
            .context("cannot run systemctl")?;
        match String::from_utf8_lossy(&out.stdout).trim() {
            "active" | "activating" => Ok(RuntimeState::Running),
            _ => Ok(RuntimeState::Stopped),
        }
    }

    fn usage(&self, meta: &AppMeta, dir: &Path) -> Result<Option<ResourceUsage>> {
        if self.state(meta, dir)? != RuntimeState::Running {
            return Ok(None);
        }
        Ok(cgroup_usage(unit(meta)?))
    }

    fn logs(&self, meta: &AppMeta, _dir: &Path, tail: usize) -> Result<String> {
        let tail = tail.to_string();
        let out = Command::new("journalctl")
            .args(["-u", unit(meta)?, "-n", &tail, "--no-pager", "-o", "cat"])
            .output()
            .context("cannot run journalctl")?;
        if !out.status.success() {
            bail!(
                "journalctl failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    }

    fn remove(&self, meta: &AppMeta, _dir: &Path) -> Result<()> {
        let unit = unit(meta)?;
        // Best effort: the unit may already be stopped/disabled.
        let _ = systemctl(&["disable", "--now", unit]);
        let path = format!("/etc/systemd/system/{unit}");
        match std::fs::remove_file(&path) {
            Ok(()) => systemctl(&["daemon-reload"]),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("cannot remove unit file {path}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_name_convention() {
        assert_eq!(unit_name("helloworld"), "asc-app-helloworld.service");
    }

    #[test]
    fn cpu_stat_usage_usec_parses() {
        let raw = "usage_usec 123456\nuser_usec 100000\nsystem_usec 23456\n";
        assert_eq!(parse_cpu_stat_usage_usec(raw), Some(123456));
        assert_eq!(parse_cpu_stat_usage_usec("nr_periods 0\n"), None);
    }
}
