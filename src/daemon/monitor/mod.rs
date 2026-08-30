//! Monitoring (DMN-006): system metrics sampled in the background, kept in
//! an in-memory ring buffer and served over the daemon API (`MonitorService`
//! plus REST `/v1/metrics`). Per-app metrics, SQLite history and the
//! platform push stream are follow-up increments (see docs/monitoring.md).

pub mod gpu;
pub mod network;
pub mod system;

use std::collections::VecDeque;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::sync::broadcast;

pub use gpu::GpuMetrics;
pub use network::{InterfaceAddress, NetworkInterface};
pub use system::SystemMetrics;

use crate::daemon::config::MonitorConfig;

/// Broadcast capacity: a handful of samples is enough slack for a subscriber
/// briefly busy encoding a websocket frame; falling further behind than this
/// is reported as `Lagged` and the subscriber just skips ahead rather than
/// blocking the sampler.
const BROADCAST_CAPACITY: usize = 16;

/// Ring buffer of recent system samples plus a live broadcast, shared
/// between the sampler task and the API. Lock scope stays tiny: clone-out on
/// read, push on write. `StreamSystemMetrics` (DMN-072) subscribes to the
/// broadcast side instead of polling `latest()`.
pub struct Monitor {
    samples: RwLock<VecDeque<SystemMetrics>>,
    capacity: usize,
    live: broadcast::Sender<SystemMetrics>,
}

impl Monitor {
    pub fn new(config: &MonitorConfig) -> Arc<Self> {
        let (live, _) = broadcast::channel(BROADCAST_CAPACITY);
        Arc::new(Self {
            samples: RwLock::new(VecDeque::with_capacity(config.history_samples)),
            capacity: config.history_samples.max(1),
            live,
        })
    }

    /// Subscribe to every sample as it is taken. The receiver reports
    /// `Lagged` if it falls more than [`BROADCAST_CAPACITY`] samples behind;
    /// callers should treat that as "skip ahead", not as an error to bubble up.
    pub fn subscribe(&self) -> broadcast::Receiver<SystemMetrics> {
        self.live.subscribe()
    }

    /// Spawn the background sampler; it stops when the daemon shuts down
    /// (the runtime drops the task). The first tick fires immediately so the
    /// API has data right after startup; usage/rate fields fill in from the
    /// second sample onward.
    pub fn start_sampler(self: &Arc<Self>, config: &MonitorConfig) {
        let monitor = Arc::clone(self);
        let interval = Duration::from_millis(config.interval_ms());
        tokio::spawn(async move {
            let mut collector = system::Collector::new();
            let mut ticker = tokio::time::interval(interval);
            // A 100ms sampler that falls behind (a slow blocking sample, a
            // busy host) should not fire a burst of catch-up ticks; it
            // should just resume at the normal cadence.
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                // procfs reads and statvfs are microseconds, but GPU
                // collection may spawn `nvidia-smi` — which is why the whole
                // sample runs on a blocking thread. The collector is moved in
                // and handed back so it keeps its previous reading, the
                // basis for CPU usage and network rates.
                let sampled = tokio::task::spawn_blocking(move || {
                    let sample = collector.sample();
                    (collector, sample)
                })
                .await;
                match sampled {
                    Ok((returned, sample)) => {
                        collector = returned;
                        match sample {
                            Ok(sample) => monitor.push(sample),
                            Err(err) => {
                                tracing::warn!(error = %format!("{err:#}"), "metrics sample failed")
                            }
                        }
                    }
                    Err(err) => {
                        // The blocking task panicked: the collector is gone
                        // with it, so start over rather than stop sampling.
                        tracing::warn!(error = %err.to_string(), "metrics sampler panicked");
                        collector = system::Collector::new();
                    }
                }
            }
        });
    }

    pub fn push(&self, sample: SystemMetrics) {
        // No receivers is the common case between panel opens; `send`
        // returning an error just means "nobody is listening right now".
        let _ = self.live.send(sample.clone());
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
            disk_io: Vec::new(),
            gpus: Vec::new(),
            uptime_secs: ts as u64,
        }
    }

    fn monitor(capacity: usize) -> Arc<Monitor> {
        Monitor::new(&MonitorConfig {
            interval_ms: Some(10_000),
            interval_secs: None,
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

    #[tokio::test]
    async fn subscribers_receive_pushed_samples() {
        let m = monitor(3);
        let mut rx = m.subscribe();
        m.push(sample(1));
        let received = rx.recv().await.unwrap();
        assert_eq!(received.timestamp, 1);
    }

    #[tokio::test]
    async fn push_with_no_subscribers_does_not_panic() {
        let m = monitor(3);
        m.push(sample(1));
        assert_eq!(m.latest().unwrap().timestamp, 1);
    }
}
