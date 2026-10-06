//! Monitoring: system metrics sampled in the background, kept in
//! an in-memory ring buffer and served over the daemon API (`MonitorService`
//! plus REST `/v1/metrics`). Per-app metrics, SQLite history and the
//! platform push stream are follow-up increments (see docs/monitoring.md).

pub mod gpu;
pub mod hardware;
pub mod network;
pub mod processes;
pub mod sensors;
pub mod sockets;
pub mod system;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::sync::{Notify, broadcast};

pub use gpu::GpuMetrics;
pub use network::{InterfaceAddress, NetworkInterface};
pub use system::SystemMetrics;

use crate::daemon::config::{
    Config, MONITOR_IDLE_INTERVAL_MS_RANGE, MONITOR_INTERVAL_MS_RANGE, MonitorConfig,
};

/// The two sampling cadences: `interval_ms` while the live stream
/// has subscribers, `idle_interval_ms` otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorSettings {
    pub interval_ms: u64,
    pub idle_interval_ms: u64,
}

impl MonitorSettings {
    /// Reject values outside the documented bounds instead of clamping them:
    /// a caller that asked for 10ms should learn it did not get it.
    pub fn validate(&self) -> Result<()> {
        if !MONITOR_INTERVAL_MS_RANGE.contains(&self.interval_ms) {
            bail!(
                "interval_ms must be within {}..={} (got {})",
                MONITOR_INTERVAL_MS_RANGE.start(),
                MONITOR_INTERVAL_MS_RANGE.end(),
                self.interval_ms
            );
        }
        if !MONITOR_IDLE_INTERVAL_MS_RANGE.contains(&self.idle_interval_ms) {
            bail!(
                "idle_interval_ms must be within {}..={} (got {})",
                MONITOR_IDLE_INTERVAL_MS_RANGE.start(),
                MONITOR_IDLE_INTERVAL_MS_RANGE.end(),
                self.idle_interval_ms
            );
        }
        if self.idle_interval_ms < self.interval_ms {
            bail!(
                "idle_interval_ms ({}) must not be below interval_ms ({})",
                self.idle_interval_ms,
                self.interval_ms
            );
        }
        Ok(())
    }
}

/// Persist new cadences into `[monitor]` of the daemon's own config.toml.
/// Re-read from disk rather than written from the in-memory
/// config: whatever else changed in the file since startup must survive.
pub fn save_settings(settings: MonitorSettings) -> Result<()> {
    let path = Config::path();
    let mut config = Config::load_from(&path)?;
    config.monitor.interval_ms = Some(settings.interval_ms);
    config.monitor.idle_interval_ms = Some(settings.idle_interval_ms);
    config.save_to(&path)
}

/// Broadcast capacity: a handful of samples is enough slack for a subscriber
/// briefly busy encoding a websocket frame; falling further behind than this
/// is reported as `Lagged` and the subscriber just skips ahead rather than
/// blocking the sampler.
const BROADCAST_CAPACITY: usize = 16;

/// Ring buffer of recent system samples plus a live broadcast, shared
/// between the sampler task and the API. Lock scope stays tiny: clone-out on
/// read, push on write. `StreamSystemMetrics` subscribes to the
/// broadcast side instead of polling `latest()`.
pub struct Monitor {
    samples: RwLock<VecDeque<SystemMetrics>>,
    capacity: usize,
    live: broadcast::Sender<SystemMetrics>,
    /// Live and idle cadences, changeable while the sampler runs.
    interval_ms: AtomicU64,
    idle_interval_ms: AtomicU64,
    /// Cuts the sampler's current wait short: a new subscriber, or new
    /// settings, should not sit out the rest of a 5s idle interval.
    wake: Notify,
}

impl Monitor {
    pub fn new(config: &MonitorConfig) -> Arc<Self> {
        let (live, _) = broadcast::channel(BROADCAST_CAPACITY);
        Arc::new(Self {
            samples: RwLock::new(VecDeque::with_capacity(config.history_samples)),
            capacity: config.history_samples.max(1),
            live,
            interval_ms: AtomicU64::new(config.interval_ms()),
            idle_interval_ms: AtomicU64::new(config.idle_interval_ms()),
            wake: Notify::new(),
        })
    }

    /// Subscribe to every sample as it is taken. The receiver reports
    /// `Lagged` if it falls more than [`BROADCAST_CAPACITY`] samples behind;
    /// callers should treat that as "skip ahead", not as an error to bubble up.
    ///
    /// Subscribing switches the sampler to the live cadence at once — it is
    /// woken rather than left to finish an idle wait.
    pub fn subscribe(&self) -> broadcast::Receiver<SystemMetrics> {
        let rx = self.live.subscribe();
        self.wake.notify_one();
        rx
    }

    /// The cadences currently in effect.
    pub fn settings(&self) -> MonitorSettings {
        MonitorSettings {
            interval_ms: self.interval_ms.load(Ordering::Relaxed),
            idle_interval_ms: self.idle_interval_ms.load(Ordering::Relaxed),
        }
    }

    /// Switch the running sampler to new cadences (validated by the caller
    /// via [`MonitorSettings::validate`]). Takes effect on the next wait.
    pub fn apply(&self, settings: MonitorSettings) {
        self.interval_ms
            .store(settings.interval_ms, Ordering::Relaxed);
        self.idle_interval_ms
            .store(settings.idle_interval_ms, Ordering::Relaxed);
        self.wake.notify_one();
    }

    /// The wait before the next sample: the live cadence while anyone is
    /// subscribed to the stream, the idle one otherwise.
    fn current_interval(&self) -> Duration {
        let settings = self.settings();
        let millis = if self.live.receiver_count() > 0 {
            settings.interval_ms
        } else {
            settings.idle_interval_ms
        };
        Duration::from_millis(millis.max(10))
    }

    /// Spawn the background sampler; it stops when the daemon shuts down
    /// (the runtime drops the task). The first sample is taken immediately
    /// so the API has data right after startup; usage/rate fields fill in
    /// from the second sample onward.
    ///
    /// Cadence is adaptive: fast only while someone watches the
    /// live stream, slow otherwise. Each wait is measured from the start of
    /// the previous sample, so a slow sample (a busy host, a stalled
    /// `statvfs`) delays the next one instead of triggering a catch-up burst.
    pub fn start_sampler(self: &Arc<Self>, _config: &MonitorConfig) {
        let monitor = Arc::clone(self);
        tokio::spawn(async move {
            let mut collector = system::Collector::new();
            let mut started = tokio::time::Instant::now();
            let mut first = true;
            loop {
                if !first {
                    let deadline = started + monitor.current_interval();
                    tokio::select! {
                        _ = tokio::time::sleep_until(deadline) => {}
                        _ = monitor.wake.notified() => {}
                    }
                }
                first = false;
                started = tokio::time::Instant::now();
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
            temperatures: Vec::new(),
            fans: Vec::new(),
            uptime_secs: ts as u64,
        }
    }

    fn monitor(capacity: usize) -> Arc<Monitor> {
        Monitor::new(&MonitorConfig {
            interval_ms: Some(10_000),
            idle_interval_ms: None,
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

    /// End to end: a config carrying only the obsolete key must still
    /// drive the sampler at the 100ms default while someone watches. Guards
    /// the sampler wiring, not just `interval_ms()` — the two were connected
    /// by a fallback that made every legacy install sample once per 10
    /// seconds.
    #[tokio::test(flavor = "multi_thread")]
    async fn legacy_config_still_samples_ten_times_a_second() {
        let config = MonitorConfig {
            interval_ms: None,
            idle_interval_ms: None,
            interval_secs: Some(10),
            history_samples: 300,
        };
        let monitor = Monitor::new(&config);
        let _watcher = monitor.subscribe();
        monitor.start_sampler(&config);
        tokio::time::sleep(Duration::from_millis(700)).await;
        let taken = monitor.history(0).len();
        assert!(
            taken >= 4,
            "expected several samples in 700ms, took {taken} — the legacy interval is back"
        );
    }

    /// nobody subscribed — the sampler takes its first sample and
    /// then idles instead of reading procfs ten times a second.
    #[tokio::test(flavor = "multi_thread")]
    async fn sampler_idles_without_subscribers() {
        let config = MonitorConfig::default();
        let monitor = Monitor::new(&config);
        monitor.start_sampler(&config);
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(
            monitor.history(0).len(),
            1,
            "an unwatched sampler kept the live cadence"
        );
    }

    /// a subscriber arriving mid-idle wakes the sampler at once
    /// rather than leaving the panel to wait out the idle interval.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_subscriber_wakes_an_idle_sampler() {
        let config = MonitorConfig::default();
        let monitor = Monitor::new(&config);
        monitor.start_sampler(&config);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut rx = monitor.subscribe();
        let woke = tokio::time::timeout(Duration::from_millis(1_000), rx.recv()).await;
        assert!(
            woke.is_ok(),
            "the sampler kept idling after a subscriber arrived"
        );
    }

    #[test]
    fn settings_apply_and_validate() {
        let m = monitor(3);
        let fast = MonitorSettings {
            interval_ms: 500,
            idle_interval_ms: 10_000,
        };
        assert!(fast.validate().is_ok());
        m.apply(fast);
        assert_eq!(m.settings(), fast);

        let too_fast = MonitorSettings {
            interval_ms: 10,
            idle_interval_ms: 10_000,
        };
        assert!(too_fast.validate().is_err());
        let inverted = MonitorSettings {
            interval_ms: 5_000,
            idle_interval_ms: 2_000,
        };
        assert!(inverted.validate().is_err());
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
