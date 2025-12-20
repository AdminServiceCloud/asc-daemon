//! Monitoring (DMN-006): system metrics sampled in the background, kept in
//! an in-memory ring buffer and served over the daemon API (`MonitorService`
//! plus REST `/v1/metrics`). Per-app metrics, SQLite history and the
//! platform push stream are follow-up increments (see docs/monitoring.md).

pub mod system;

use std::collections::VecDeque;
use std::sync::{Arc, RwLock};
use std::time::Duration;

pub use system::SystemMetrics;

use crate::daemon::config::MonitorConfig;

/// Ring buffer of recent system samples, shared between the sampler task and
/// the API. Lock scope stays tiny: clone-out on read, push on write.
pub struct Monitor {
    samples: RwLock<VecDeque<SystemMetrics>>,
    capacity: usize,
}

impl Monitor {
    pub fn new(config: &MonitorConfig) -> Arc<Self> {
        Arc::new(Self {
            samples: RwLock::new(VecDeque::with_capacity(config.history_samples)),
            capacity: config.history_samples.max(1),
        })
    }

    /// Spawn the background sampler; it stops when the daemon shuts down
    /// (the runtime drops the task). The first tick fires immediately so the
    /// API has data right after startup; usage/rate fields fill in from the
    /// second sample onward.
    pub fn start_sampler(self: &Arc<Self>, config: &MonitorConfig) {
        let monitor = Arc::clone(self);
        let interval = Duration::from_secs(config.interval_secs.max(1));
        tokio::spawn(async move {
            let mut collector = system::Collector::new();
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                // procfs reads and statvfs are microseconds — no need to
                // leave the async context for them.
                match collector.sample() {
                    Ok(sample) => monitor.push(sample),
                    Err(err) => {
                        tracing::warn!(error = %format!("{err:#}"), "metrics sample failed")
                    }
                }
            }
        });
    }

    pub fn push(&self, sample: SystemMetrics) {
        let mut samples = self.samples.write().expect("metrics lock poisoned");
        if samples.len() == self.capacity {
            samples.pop_front();
        }
        samples.push_back(sample);
    }

    /// Most recent sample, if any was taken yet.
    pub fn latest(&self) -> Option<SystemMetrics> {
        self.samples
            .read()
            .expect("metrics lock poisoned")
            .back()
            .cloned()
    }

    /// Up to `limit` most recent samples, oldest first (0 = everything).
    pub fn history(&self, limit: usize) -> Vec<SystemMetrics> {
        let samples = self.samples.read().expect("metrics lock poisoned");
        let skip = if limit == 0 {
            0
        } else {
            samples.len().saturating_sub(limit)
        };
        samples.iter().skip(skip).cloned().collect()
    }
}

/// "15.6 GiB"-style size for terminal output.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_picks_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(16 * 1024 * 1024 * 1024), "16.0 GiB");
    }

    fn sample(ts: i64) -> SystemMetrics {
        SystemMetrics {
            timestamp: ts,
            cpu: system::CpuMetrics {
                usage_percent: None,
                cores: 1,
                load1: 0.0,
                load5: 0.0,
                load15: 0.0,
            },
            memory: system::MemoryMetrics {
                total: 1,
                used: 0,
                available: 1,
                swap_total: 0,
                swap_used: 0,
            },
            disks: Vec::new(),
            network: Vec::new(),
            uptime_secs: ts as u64,
        }
    }

    fn monitor(capacity: usize) -> Arc<Monitor> {
        Monitor::new(&MonitorConfig {
            interval_secs: 10,
            history_samples: capacity,
        })
    }

    #[test]
    fn ring_buffer_drops_oldest() {
        let m = monitor(3);
        for ts in 1..=5 {
            m.push(sample(ts));
        }
        let history = m.history(0);
        let stamps: Vec<i64> = history.iter().map(|s| s.timestamp).collect();
        assert_eq!(stamps, vec![3, 4, 5]);
        assert_eq!(m.latest().unwrap().timestamp, 5);
    }

    #[test]
    fn history_limit_returns_most_recent() {
        let m = monitor(10);
        for ts in 1..=5 {
            m.push(sample(ts));
        }
        let stamps: Vec<i64> = m.history(2).iter().map(|s| s.timestamp).collect();
        assert_eq!(stamps, vec![4, 5]);
    }

    #[test]
    fn empty_monitor_has_no_latest() {
        assert!(monitor(3).latest().is_none());
        assert!(monitor(3).history(0).is_empty());
    }
}
