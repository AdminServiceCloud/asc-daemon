//! Resource shortfall check before an install actually provisions anything
//! (DMN-099).
//!
//! "Not enough resources" hides two very different failure shapes. RAM and
//! disk running low is advisory — a human (or `--force`) can decide to
//! proceed the same way `asc app start`'s own resource check already lets
//! them, and nothing downstream enforces those numbers at container-create
//! time. A CPU quota above the host's total core count is not advisory at
//! all: the Engine rejects `NanoCpus` above `nproc` outright (`Docker
//! responded with status code 400: range of CPUs is from 0.01 to <cores>`),
//! and no amount of consent changes that — the only way through is capping
//! the quota, which [`clamp_cpu`] does unconditionally, `--force` or not.

use std::path::Path;

use crate::daemon::apps::meta::Quota;
use crate::daemon::i18n::{Msg, tf2};
use crate::daemon::monitor;
use crate::daemon::monitor::system::SystemMetrics;
use crate::daemon::progress::InstallReporter;

use super::manifest::Requirements;
use super::settings::parse_size;

/// One resource the host falls short on, as `need > have` — RAM/disk in
/// human-readable sizes, CPU in cores.
#[derive(Debug, Clone)]
pub struct Shortage {
    /// "RAM", "disk" or "CPU".
    pub resource: String,
    pub need: String,
    pub have: String,
}

impl std::fmt::Display for Shortage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {} > {}", self.resource, self.need, self.have)
    }
}

/// Typed error: the install would ask the host for more than it currently
/// has. Not raised when the caller passed `force` — see [`check`]'s callers.
/// Mirrors [`super::install::LicenseRequired`]'s shape: a structured detail
/// the CLI turns into an interactive prompt and the platform turns into its
/// own "not enough resources, install anyway?" screen.
#[derive(Debug)]
pub struct RequirementsNotMet {
    pub app: String,
    pub shortages: Vec<Shortage>,
}

impl std::fmt::Display for RequirementsNotMet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let list = self
            .shortages
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        write!(f, "{}", tf2(Msg::PkgResourcesInsufficient, &self.app, list))
    }
}

impl std::error::Error for RequirementsNotMet {}

/// Shortfall of the manifest's declared `requirements` and the runtime
/// `quota` against what the host currently has. RAM/disk compare against
/// what is free right now (apps already running on the host count against
/// it); CPU compares against the host's **total** core count, not its
/// current load — cores are not consumed the way RAM is, and total capacity
/// is what predicts the Engine's own validation.
pub fn check(
    requirements: Option<&Requirements>,
    quota: Option<&Quota>,
    metrics: &SystemMetrics,
    app_dir: &Path,
) -> Vec<Shortage> {
    let mut shortages = Vec::new();

    if let Some(need) = requirements
        .and_then(|r| r.ram.as_deref())
        .and_then(|s| parse_size(s).ok())
        && need > metrics.memory.available
    {
        shortages.push(Shortage {
            resource: "RAM".to_string(),
            need: monitor::human_bytes(need),
            have: monitor::human_bytes(metrics.memory.available),
        });
    }

    if let Some(need) = requirements
        .and_then(|r| r.disk.as_deref())
        .and_then(|s| parse_size(s).ok())
    {
        // The filesystem the app directory lives on: longest matching mount.
        let disk = metrics
            .disks
            .iter()
            .filter(|d| app_dir.starts_with(&d.mount))
            .max_by_key(|d| d.mount.len());
        if let Some(disk) = disk
            && need > disk.available
        {
            shortages.push(Shortage {
                resource: "disk".to_string(),
                need: monitor::human_bytes(need),
                have: format!("{} ({})", monitor::human_bytes(disk.available), disk.mount),
            });
        }
    }

    // The higher of the manifest's advisory `requirements.cpu` and the
    // actual runtime quota: the quota is what reaches the Engine's
    // `NanoCpus`, so its overshoot is the one the caller most needs to know
    // about before hitting a hard container-create failure.
    let need_cpu = [
        requirements.and_then(|r| r.cpu),
        quota.and_then(|q| q.cpu_cores),
    ]
    .into_iter()
    .flatten()
    .fold(0.0_f64, f64::max);
    if need_cpu > 0.0 && need_cpu > metrics.cpu.cores as f64 {
        shortages.push(Shortage {
            resource: "CPU".to_string(),
            need: need_cpu.to_string(),
            have: metrics.cpu.cores.to_string(),
        });
    }

    shortages
}

/// Cap a quota's CPU limit to the host's total logical core count. Real
/// clamping only — never invents a quota that was not there, and never
/// raises one. `report`, when given, gets one line describing the cap;
/// otherwise it goes to the daemon's own log.
pub fn clamp_cpu(
    quota: Option<Quota>,
    host_cores: u32,
    app: &str,
    report: Option<&dyn InstallReporter>,
) -> Option<Quota> {
    let mut quota = quota?;
    if let Some(cores) = quota.cpu_cores
        && cores > host_cores as f64
    {
        let message = format!(
            "warning: '{app}' asked for {cores} CPU cores, the host only has {host_cores}; capping the quota to {host_cores}"
        );
        match report {
            Some(report) => report.line(&message),
            None => tracing::warn!(app, requested = cores, host_cores, "{message}"),
        }
        quota.cpu_cores = Some(host_cores as f64);
    }
    Some(quota)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn metrics(cores: u32, ram_available: u64, disk_available: u64) -> SystemMetrics {
        SystemMetrics {
            timestamp: 0,
            cpu: monitor::system::CpuMetrics {
                usage_percent: None,
                cores,
                load1: 0.0,
                load5: 0.0,
                load15: 0.0,
            },
            memory: monitor::system::MemoryMetrics {
                total: ram_available,
                used: 0,
                available: ram_available,
                swap_total: 0,
                swap_used: 0,
            },
            disks: vec![monitor::system::DiskMetrics {
                mount: "/".to_string(),
                filesystem: "ext4".to_string(),
                total: disk_available,
                used: 0,
                available: disk_available,
            }],
            network: Vec::new(),
            disk_io: Vec::new(),
            gpus: Vec::new(),
            uptime_secs: 0,
        }
    }

    fn quota(cpu_cores: f64) -> Quota {
        Quota {
            cpu_cores: Some(cpu_cores),
            ram_bytes: None,
            disk_bytes: None,
        }
    }

    #[test]
    fn no_shortage_when_the_host_covers_everything() {
        let requirements = Requirements {
            ram: Some("1G".to_string()),
            disk: Some("1G".to_string()),
            cpu: Some(1.0),
        };
        let metrics = metrics(4, 8 << 30, 100 << 30);
        let shortages = check(
            Some(&requirements),
            Some(&quota(1.0)),
            &metrics,
            &PathBuf::from("/"),
        );
        assert!(shortages.is_empty(), "got: {shortages:?}");
    }

    #[test]
    fn requirements_cpu_shortfall_is_reported() {
        let requirements = Requirements {
            ram: None,
            disk: None,
            cpu: Some(4.0),
        };
        let metrics = metrics(1, 8 << 30, 100 << 30);
        let shortages = check(Some(&requirements), None, &metrics, &PathBuf::from("/"));
        assert_eq!(shortages.len(), 1);
        assert_eq!(shortages[0].resource, "CPU");
        assert_eq!(shortages[0].need, "4");
        assert_eq!(shortages[0].have, "1");
    }

    /// The scenario from the cs2 install failure: the manifest declares no
    /// `requirements.cpu`, but the runtime `quota` (asc.settings.yaml) asks
    /// for more cores than the host has — this is what would otherwise reach
    /// the Engine as a raw `NanoCpus` rejection.
    #[test]
    fn quota_cpu_shortfall_is_reported_even_without_a_requirements_section() {
        let metrics = metrics(1, 8 << 30, 100 << 30);
        let shortages = check(None, Some(&quota(2.0)), &metrics, &PathBuf::from("/"));
        assert_eq!(shortages.len(), 1);
        assert_eq!(shortages[0].resource, "CPU");
        assert_eq!(shortages[0].need, "2");
        assert_eq!(shortages[0].have, "1");
    }

    #[test]
    fn ram_and_disk_shortfalls_are_reported_independently() {
        let requirements = Requirements {
            ram: Some("4G".to_string()),
            disk: Some("80G".to_string()),
            cpu: None,
        };
        let metrics = metrics(4, 1 << 30, 10 << 30);
        let shortages = check(Some(&requirements), None, &metrics, &PathBuf::from("/"));
        let resources: Vec<&str> = shortages.iter().map(|s| s.resource.as_str()).collect();
        assert_eq!(resources, ["RAM", "disk"]);
    }

    #[test]
    fn clamp_cpu_caps_a_quota_above_host_capacity() {
        let clamped = clamp_cpu(Some(quota(4.0)), 1, "demo", None).unwrap();
        assert_eq!(clamped.cpu_cores, Some(1.0));
    }

    #[test]
    fn clamp_cpu_leaves_a_quota_within_capacity_untouched() {
        let clamped = clamp_cpu(Some(quota(0.5)), 4, "demo", None).unwrap();
        assert_eq!(clamped.cpu_cores, Some(0.5));
    }

    #[test]
    fn clamp_cpu_is_a_no_op_without_a_quota() {
        assert!(clamp_cpu(None, 1, "demo", None).is_none());
    }
}
