//! System metrics collection from procfs and statvfs — no external crates.
//!
//! Parsers are pure functions over `&str` so they are unit-testable without
//! a live `/proc`. Linux-only for now; a macOS collector will live behind
//! the same [`SystemMetrics`] shape later.

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::gpu::{Detector as GpuDetector, GpuMetrics};

/// One snapshot of system-wide metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemMetrics {
    /// Unix seconds when the sample was taken.
    pub timestamp: i64,
    pub cpu: CpuMetrics,
    pub memory: MemoryMetrics,
    pub disks: Vec<DiskMetrics>,
    pub network: Vec<NetworkMetrics>,
    /// Per-device disk I/O; empty when `/proc/diskstats` cannot be read.
    pub disk_io: Vec<DiskIoMetrics>,
    /// Every GPU the machine exposes; empty on the usual headless server.
    pub gpus: Vec<GpuMetrics>,
    pub uptime_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuMetrics {
    /// Busy share of all cores since the previous sample, 0–100.
    /// `None` on the very first sample (usage needs two readings).
    pub usage_percent: Option<f64>,
    pub cores: u32,
    pub load1: f64,
    pub load5: f64,
    pub load15: f64,
}

/// All sizes in bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryMetrics {
    pub total: u64,
    /// total - available: what applications actually occupy.
    pub used: u64,
    pub available: u64,
    pub swap_total: u64,
    pub swap_used: u64,
}

/// One mounted real filesystem; sizes in bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskMetrics {
    pub mount: String,
    pub filesystem: String,
    pub total: u64,
    pub used: u64,
    pub available: u64,
}

/// One network interface (loopback excluded). Counters are cumulative since
/// boot; per-second rates are deltas against the previous sample.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkMetrics {
    pub interface: String,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_errors: u64,
    pub tx_errors: u64,
    pub rx_bytes_per_sec: Option<f64>,
    pub tx_bytes_per_sec: Option<f64>,
}

/// One block device from `/proc/diskstats`. Byte counters are cumulative
/// since boot (sectors * 512); per-second rates are deltas against the
/// previous sample, `None` on the first one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskIoMetrics {
    pub device: String,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub read_bytes_per_sec: Option<f64>,
    pub write_bytes_per_sec: Option<f64>,
    /// Milliseconds spent doing I/O (field 13, cumulative since boot).
    pub io_ms: u64,
}

/// Aggregate CPU time counters from `/proc/stat` (USER_HZ ticks).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuTimes {
    pub busy: u64,
    pub total: u64,
}

impl CpuTimes {
    /// Busy share between two readings, 0–100. `None` when no time passed.
    pub fn usage_since(&self, earlier: &CpuTimes) -> Option<f64> {
        let total = self.total.checked_sub(earlier.total)?;
        if total == 0 {
            return None;
        }
        let busy = self.busy.saturating_sub(earlier.busy);
        Some(busy as f64 / total as f64 * 100.0)
    }
}

/// Collector holding the previous reading so CPU usage and network rates can
/// be computed as deltas. One instance lives inside the daemon's sampler.
#[derive(Default)]
pub struct Collector {
    prev_cpu: Option<CpuTimes>,
    /// (unix seconds, per-interface cumulative counters) of the last sample.
    prev_net: Option<(i64, Vec<NetworkMetrics>)>,
    /// (unix seconds, per-device cumulative counters) of the last sample.
    prev_disk_io: Option<(i64, Vec<DiskIoMetrics>)>,
    /// Remembers which GPU sources this machine has, so a server without one
    /// never spawns `nvidia-smi` twice.
    gpus: GpuDetector,
    /// Millisecond clock and last reading of the GPU poll. `nvidia-smi` is a
    /// subprocess spawn — sampling it on every 100ms tick would pile up
    /// blocking work, so it is refreshed on its own slower cadence and the
    /// last known reading is repeated in between.
    last_gpu_poll_ms: Option<i64>,
    cached_gpus: Vec<GpuMetrics>,
}

/// Minimum gap between two GPU polls. Fast enough that a graph feels live,
/// slow enough that `nvidia-smi` never overlaps itself under a 100ms sampler.
const GPU_POLL_MIN_INTERVAL_MS: i64 = 1000;

impl Collector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take one snapshot. Never fails outright on a single unreadable source:
    /// disks/network/GPU degrade to empty lists, but /proc/stat and
    /// /proc/meminfo are mandatory — without them the sample is meaningless.
    ///
    /// Blocking: GPU collection may run `nvidia-smi`, so the daemon calls
    /// this from a blocking task rather than straight from the sampler.
    pub fn sample(&mut self) -> Result<SystemMetrics> {
        let timestamp = unix_now();
        let timestamp_ms = unix_now_ms();

        let stat = fs::read_to_string("/proc/stat").context("cannot read /proc/stat")?;
        let cpu_times = parse_cpu_times(&stat).context("cannot parse /proc/stat")?;
        let cores = parse_core_count(&stat);
        let usage_percent = self
            .prev_cpu
            .as_ref()
            .and_then(|prev| cpu_times.usage_since(prev));
        self.prev_cpu = Some(cpu_times);

        let loadavg = fs::read_to_string("/proc/loadavg").unwrap_or_default();
        let (load1, load5, load15) = parse_loadavg(&loadavg).unwrap_or((0.0, 0.0, 0.0));

        let meminfo = fs::read_to_string("/proc/meminfo").context("cannot read /proc/meminfo")?;
        let memory = parse_meminfo(&meminfo).context("cannot parse /proc/meminfo")?;

        let mut network = fs::read_to_string("/proc/net/dev")
            .ok()
            .map(|raw| parse_net_dev(&raw))
            .unwrap_or_default();
        if let Some((prev_ts_ms, prev)) = &self.prev_net {
            let elapsed = (timestamp_ms - prev_ts_ms) as f64 / 1000.0;
            if elapsed > 0.0 {
                for iface in &mut network {
                    if let Some(p) = prev.iter().find(|p| p.interface == iface.interface) {
                        iface.rx_bytes_per_sec =
                            Some(iface.rx_bytes.saturating_sub(p.rx_bytes) as f64 / elapsed);
                        iface.tx_bytes_per_sec =
                            Some(iface.tx_bytes.saturating_sub(p.tx_bytes) as f64 / elapsed);
                    }
                }
            }
        }
        self.prev_net = Some((timestamp_ms, network.clone()));

        let mut disk_io = fs::read_to_string("/proc/diskstats")
            .ok()
            .map(|raw| parse_diskstats(&raw))
            .unwrap_or_default();
        if let Some((prev_ts_ms, prev)) = &self.prev_disk_io {
            let elapsed = (timestamp_ms - prev_ts_ms) as f64 / 1000.0;
            if elapsed > 0.0 {
                for device in &mut disk_io {
                    if let Some(p) = prev.iter().find(|p| p.device == device.device) {
                        device.read_bytes_per_sec =
                            Some(device.read_bytes.saturating_sub(p.read_bytes) as f64 / elapsed);
                        device.write_bytes_per_sec =
                            Some(device.write_bytes.saturating_sub(p.write_bytes) as f64 / elapsed);
                    }
                }
            }
        }
        self.prev_disk_io = Some((timestamp_ms, disk_io.clone()));

        // nvidia-smi is a subprocess spawn; polled on its own slower cadence
        // (see GPU_POLL_MIN_INTERVAL_MS) so a 100ms sampler never piles up
        // blocking work waiting on it. The reading in between is repeated.
        let due = self
            .last_gpu_poll_ms
            .is_none_or(|last| timestamp_ms - last >= GPU_POLL_MIN_INTERVAL_MS);
        if due {
            self.cached_gpus = self.gpus.collect();
            self.last_gpu_poll_ms = Some(timestamp_ms);
        }

        let uptime = fs::read_to_string("/proc/uptime").unwrap_or_default();
        let uptime_secs = parse_uptime(&uptime).unwrap_or(0);

        Ok(SystemMetrics {
            timestamp,
            cpu: CpuMetrics {
                usage_percent,
                cores,
                load1,
                load5,
                load15,
            },
            memory,
            disks: collect_disks(),
            network,
            disk_io,
            gpus: self.cached_gpus.clone(),
            uptime_secs,
        })
    }
}

/// One-shot snapshot with a real CPU usage figure: reads twice with a short
/// pause. Used by the CLI (`asc status`); the daemon uses [`Collector`].
pub fn snapshot_blocking() -> Result<SystemMetrics> {
    let mut collector = Collector::new();
    collector.sample()?;
    std::thread::sleep(std::time::Duration::from_millis(250));
    collector.sample()
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Millisecond clock for rate deltas. A 100ms sampler needs sub-second
/// resolution — `unix_now()` alone would see the same second across several
/// consecutive samples and report a zero elapsed time.
fn unix_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Parse the aggregate `cpu ` line of `/proc/stat`.
///
/// Fields: user nice system idle iowait irq softirq steal [guest guest_nice].
/// Busy excludes idle and iowait; guest time is already included in user.
pub fn parse_cpu_times(stat: &str) -> Option<CpuTimes> {
    let line = stat.lines().find(|l| l.starts_with("cpu "))?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .take(8)
        .filter_map(|f| f.parse().ok())
        .collect();
    if fields.len() < 4 {
        return None;
    }
    let total: u64 = fields.iter().sum();
    let idle = fields[3] + fields.get(4).copied().unwrap_or(0);
    Some(CpuTimes {
        busy: total - idle,
        total,
    })
}

/// Count `cpuN` lines of `/proc/stat` (present even when offline masks vary).
pub fn parse_core_count(stat: &str) -> u32 {
    stat.lines()
        .filter(|l| l.starts_with("cpu") && l.as_bytes().get(3).is_some_and(|b| b.is_ascii_digit()))
        .count() as u32
}

/// First three floats of `/proc/loadavg`.
pub fn parse_loadavg(raw: &str) -> Option<(f64, f64, f64)> {
    let mut it = raw.split_whitespace().filter_map(|f| f.parse::<f64>().ok());
    Some((it.next()?, it.next()?, it.next()?))
}

/// Extract totals from `/proc/meminfo` (values are in kB).
pub fn parse_meminfo(raw: &str) -> Option<MemoryMetrics> {
    let field = |name: &str| -> Option<u64> {
        raw.lines()
            .find(|l| l.starts_with(name))?
            .split_whitespace()
            .nth(1)?
            .parse::<u64>()
            .ok()
            .map(|kb| kb * 1024)
    };
    let total = field("MemTotal:")?;
    // MemAvailable exists since Linux 3.14; fall back to MemFree on ancient kernels.
    let available = field("MemAvailable:").or_else(|| field("MemFree:"))?;
    let swap_total = field("SwapTotal:").unwrap_or(0);
    let swap_free = field("SwapFree:").unwrap_or(0);
    Some(MemoryMetrics {
        total,
        used: total.saturating_sub(available),
        available,
        swap_total,
        swap_used: swap_total.saturating_sub(swap_free),
    })
}

/// Parse `/proc/net/dev`, skipping the loopback interface.
pub fn parse_net_dev(raw: &str) -> Vec<NetworkMetrics> {
    raw.lines()
        .skip(2) // two header lines
        .filter_map(|line| {
            let (name, counters) = line.split_once(':')?;
            let name = name.trim();
            if name == "lo" {
                return None;
            }
            let f: Vec<u64> = counters
                .split_whitespace()
                .map(|v| v.parse().unwrap_or(0))
                .collect();
            // rx: bytes packets errs drop ... (8 fields), then tx: same 8.
            if f.len() < 12 {
                return None;
            }
            Some(NetworkMetrics {
                interface: name.to_string(),
                rx_bytes: f[0],
                rx_errors: f[2],
                tx_bytes: f[8],
                tx_errors: f[10],
                rx_bytes_per_sec: None,
                tx_bytes_per_sec: None,
            })
        })
        .collect()
}

/// Parse `/proc/diskstats`, keeping whole block devices and skipping loop
/// devices and partitions of a disk already present in the file (`sda1` when
/// `sda` is listed, `nvme0n1p1` when `nvme0n1` is listed).
///
/// Fields (space-separated, 1-indexed per the kernel docs): 1 major,
/// 2 minor, 3 device, 4 reads completed, 5 reads merged, 6 sectors read,
/// 7 ms reading, 8 writes completed, 9 writes merged, 10 sectors written,
/// 11 ms writing, 12 I/Os in progress, 13 ms doing I/O, 14 weighted ms.
pub fn parse_diskstats(raw: &str) -> Vec<DiskIoMetrics> {
    const SECTOR_BYTES: u64 = 512;
    let rows: Vec<(&str, u64, u64, u64)> = raw
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 14 {
                return None;
            }
            let name = fields[2];
            let sectors_read: u64 = fields[5].parse().ok()?;
            let sectors_written: u64 = fields[9].parse().ok()?;
            let io_ms: u64 = fields[12].parse().ok()?;
            Some((name, sectors_read, sectors_written, io_ms))
        })
        .collect();
    let names: std::collections::HashSet<&str> = rows.iter().map(|r| r.0).collect();
    rows.into_iter()
        .filter(|(name, ..)| !is_virtual_or_partition(name, &names))
        .map(
            |(name, sectors_read, sectors_written, io_ms)| DiskIoMetrics {
                device: name.to_string(),
                read_bytes: sectors_read * SECTOR_BYTES,
                write_bytes: sectors_written * SECTOR_BYTES,
                read_bytes_per_sec: None,
                write_bytes_per_sec: None,
                io_ms,
            },
        )
        .collect()
}

/// True for loopback/ramdisk devices and for a device name that is a
/// partition of another whole disk already in `all` — `sda1` when `sda` is
/// present, `nvme0n1p1` when `nvme0n1` is present.
fn is_virtual_or_partition(name: &str, all: &std::collections::HashSet<&str>) -> bool {
    if name.starts_with("loop") || name.starts_with("ram") || name.starts_with("zram") {
        return true;
    }
    // nvme0n1p1 / mmcblk0p1 -> base is everything before the trailing "pN".
    if let Some(pos) = name.rfind('p') {
        let suffix = &name[pos + 1..];
        if !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()) {
            let base = &name[..pos];
            if (base.starts_with("nvme") || base.starts_with("mmcblk")) && all.contains(base) {
                return true;
            }
        }
    }
    // sda1 / vdb2 / xvda1 -> base is the name with trailing digits stripped.
    let base = name.trim_end_matches(|c: char| c.is_ascii_digit());
    base != name && all.contains(base)
}

/// Whole seconds of `/proc/uptime`.
pub fn parse_uptime(raw: &str) -> Option<u64> {
    raw.split_whitespace()
        .next()?
        .parse::<f64>()
        .ok()
        .map(|s| s as u64)
}

/// Mounted real filesystems (device under /dev), deduplicated by device so
/// bind mounts and btrfs subvolumes do not double-count.
fn collect_disks() -> Vec<DiskMetrics> {
    let Ok(mounts) = fs::read_to_string("/proc/self/mounts") else {
        return Vec::new();
    };
    let mut seen_devices: Vec<String> = Vec::new();
    let mut disks = Vec::new();
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let (Some(device), Some(mount), Some(fstype)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if !device.starts_with("/dev/") || seen_devices.iter().any(|d| d == device) {
            continue;
        }
        // Mount points with spaces are octal-escaped in /proc/self/mounts
        // (e.g. \040); such exotic mounts are skipped rather than misreported.
        if mount.contains('\\') {
            continue;
        }
        if let Some(mut disk) = statvfs(Path::new(mount)) {
            seen_devices.push(device.to_string());
            disk.mount = mount.to_string();
            disk.filesystem = fstype.to_string();
            disks.push(disk);
        }
    }
    disks
}

/// Total capacity of the filesystem holding `path`, for a usage bar against
/// an arbitrary directory (e.g. the apps store root in `asc disk`) rather
/// than a whole mounted device — `path` need not be a mount point itself.
pub fn filesystem_total(path: &Path) -> Option<u64> {
    statvfs(path).map(|disk| disk.total)
}

/// Filesystem usage via statvfs(3). Sizes use `f_frsize` per POSIX.
fn statvfs(mount: &Path) -> Option<DiskMetrics> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(mount.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let frsize = stat.f_frsize as u64;
    let total = stat.f_blocks as u64 * frsize;
    if total == 0 {
        return None; // pseudo-filesystem behind a /dev device — not a disk
    }
    Some(DiskMetrics {
        mount: String::new(),
        filesystem: String::new(),
        total,
        used: total - stat.f_bfree as u64 * frsize,
        // f_bavail: what unprivileged users can actually allocate.
        available: stat.f_bavail as u64 * frsize,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT: &str = "\
cpu  100 0 50 800 50 0 0 0 0 0
cpu0 50 0 25 400 25 0 0 0 0 0
cpu1 50 0 25 400 25 0 0 0 0 0
intr 12345
ctxt 6789
";

    #[test]
    fn cpu_times_and_cores() {
        let times = parse_cpu_times(STAT).unwrap();
        assert_eq!(times.total, 1000);
        assert_eq!(times.busy, 150); // total - idle(800) - iowait(50)
        assert_eq!(parse_core_count(STAT), 2);
    }

    #[test]
    fn cpu_usage_is_a_delta() {
        let t0 = CpuTimes {
            busy: 150,
            total: 1000,
        };
        let t1 = CpuTimes {
            busy: 250,
            total: 1400,
        };
        let usage = t1.usage_since(&t0).unwrap();
        assert!((usage - 25.0).abs() < 1e-9); // 100 busy of 400 total
        assert_eq!(t0.usage_since(&t0), None); // no time passed
        assert_eq!(t0.usage_since(&t1), None); // counters went backwards
    }

    #[test]
    fn loadavg_parses() {
        assert_eq!(
            parse_loadavg("0.52 0.58 0.59 1/467 12345\n"),
            Some((0.52, 0.58, 0.59))
        );
        assert_eq!(parse_loadavg(""), None);
    }

    #[test]
    fn meminfo_parses_and_computes_used() {
        let raw = "\
MemTotal:       16384000 kB
MemFree:         1024000 kB
MemAvailable:    8192000 kB
Buffers:          512000 kB
SwapTotal:       2048000 kB
SwapFree:        2000000 kB
";
        let mem = parse_meminfo(raw).unwrap();
        assert_eq!(mem.total, 16384000 * 1024);
        assert_eq!(mem.available, 8192000 * 1024);
        assert_eq!(mem.used, (16384000 - 8192000) * 1024);
        assert_eq!(mem.swap_used, 48000 * 1024);
    }

    #[test]
    fn meminfo_falls_back_to_memfree() {
        let raw = "MemTotal: 1000 kB\nMemFree: 400 kB\n";
        let mem = parse_meminfo(raw).unwrap();
        assert_eq!(mem.available, 400 * 1024);
        assert_eq!(mem.used, 600 * 1024);
    }

    #[test]
    fn net_dev_skips_loopback_and_headers() {
        let raw = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo:  111111     100    0    0    0     0          0         0   111111     100    0    0    0     0       0          0
  eth0: 5000000    4000    2    1    0     0          0         0  3000000    2500    3    0    0     0       0          0
";
        let ifaces = parse_net_dev(raw);
        assert_eq!(ifaces.len(), 1);
        let eth = &ifaces[0];
        assert_eq!(eth.interface, "eth0");
        assert_eq!(eth.rx_bytes, 5_000_000);
        assert_eq!(eth.rx_errors, 2);
        assert_eq!(eth.tx_bytes, 3_000_000);
        assert_eq!(eth.tx_errors, 3);
    }

    #[test]
    fn diskstats_skips_partitions_and_loop_devices() {
        let raw = "\
   8       0 sda 1000 0 200000 500 2000 0 400000 900 0 1200 1400
   8       1 sda1 900 0 180000 400 1800 0 380000 800 0 1100 1200
 259       0 nvme0n1 3000 0 600000 700 4000 0 800000 1100 0 1700 1900
 259       1 nvme0n1p1 2900 0 580000 650 3900 0 780000 1050 0 1650 1850
   7       0 loop0 10 0 200 5 0 0 0 0 0 5 5
";
        let disks = parse_diskstats(raw);
        let names: Vec<&str> = disks.iter().map(|d| d.device.as_str()).collect();
        assert_eq!(names, vec!["sda", "nvme0n1"]);
        let sda = disks.iter().find(|d| d.device == "sda").unwrap();
        assert_eq!(sda.read_bytes, 200_000 * 512);
        assert_eq!(sda.write_bytes, 400_000 * 512);
        assert_eq!(sda.io_ms, 1200);
        assert!(sda.read_bytes_per_sec.is_none());
    }

    #[test]
    fn diskstats_keeps_standalone_device_without_partitions() {
        let raw = "   8       0 md0 100 0 20000 50 200 0 40000 90 0 120 140\n";
        let disks = parse_diskstats(raw);
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].device, "md0");
    }

    #[test]
    fn uptime_parses_whole_seconds() {
        assert_eq!(parse_uptime("12345.67 45678.90\n"), Some(12345));
        assert_eq!(parse_uptime(""), None);
    }

    // Exercises the real /proc on Linux (the only supported build target).
    #[test]
    fn live_sample_has_sane_values() {
        let mut collector = Collector::new();
        let first = collector.sample().unwrap();
        assert!(first.cpu.cores >= 1);
        assert!(first.memory.total > 0);
        assert!(first.memory.available <= first.memory.total);
        assert!(first.uptime_secs > 0);
        assert!(first.cpu.usage_percent.is_none()); // needs two readings
        let second = collector.sample().unwrap();
        if let Some(usage) = second.cpu.usage_percent {
            assert!((0.0..=100.0).contains(&usage));
        }
    }
}
