//! GPU metrics (DMN-006).
//!
//! Two sources, both optional and both cheap to rule out:
//!
//!   - **NVIDIA** — `nvidia-smi` in CSV mode. There is no kernel interface
//!     that exposes utilisation for the proprietary driver, so the vendor
//!     tool is the only portable option; it ships with every driver package.
//!   - **AMD** — the `amdgpu` sysfs interface (`gpu_busy_percent`,
//!     `mem_info_vram_*`, hwmon), read like every other metric here.
//!
//! Machines without a GPU are the common case, so the sources are probed once
//! and a negative result is remembered: no process is spawned on every tick
//! of a server that has nothing to report.
//!
//! Parsers are pure functions over `&str` so they stay unit-testable without
//! the hardware.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// One GPU. Memory sizes are in bytes; fields the source cannot report stay
/// `None` rather than being faked with a zero.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuMetrics {
    /// Position within its vendor's enumeration (nvidia-smi index, DRM card
    /// number), stable for as long as the machine is up.
    pub index: u32,
    /// "nvidia" or "amd" — lower case, matching what the platform expects.
    pub vendor: String,
    pub name: String,
    /// Busy share of the graphics engine, 0–100.
    pub utilization_percent: Option<f64>,
    pub memory_total: u64,
    pub memory_used: u64,
    pub temperature_c: Option<f64>,
    pub power_watts: Option<f64>,
}

/// Where a machine's GPU figures come from. Probed once by [`Detector`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Nvidia,
    /// One `amdgpu` card, addressed by its `/sys/class/drm/cardN/device` path.
    Amd(PathBuf),
}

/// How long `nvidia-smi` may take before the sample gives up on it. A driver
/// stuck in an uninterruptible wait must not stall the sampler forever.
const NVIDIA_TIMEOUT: Duration = Duration::from_secs(5);

const NVIDIA_QUERY: &str =
    "index,name,utilization.gpu,memory.total,memory.used,temperature.gpu,power.draw";

/// Collects GPU samples, remembering which sources exist on this machine.
///
/// Blocking by design: it reads sysfs and may spawn `nvidia-smi`. The daemon
/// calls it from a blocking task, the CLI straight from `asc status`.
#[derive(Default)]
pub struct Detector {
    /// `None` until the first sample; an empty vector means "no GPU here",
    /// which is what stops the probing.
    sources: Option<Vec<Source>>,
}

impl Detector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sample every GPU found on the machine. Never fails: a source that
    /// stops answering drops out of the result instead of failing the whole
    /// system sample.
    pub fn collect(&mut self) -> Vec<GpuMetrics> {
        let sources = self.sources.get_or_insert_with(detect_sources);
        let mut gpus = Vec::new();
        for source in sources.iter() {
            match source {
                Source::Nvidia => gpus.extend(collect_nvidia()),
                Source::Amd(device) => gpus.extend(collect_amd(device)),
            }
        }
        gpus
    }
}

/// Probe both vendors once. Detection is deliberately cheap and read-only:
/// a card that appears after boot (hot-plugged eGPU) is not something a
/// server daemon needs to notice mid-run.
fn detect_sources() -> Vec<Source> {
    let mut sources = Vec::new();
    // The device node exists whenever the proprietary driver is loaded, so
    // this rules NVIDIA out without spawning anything on the usual server.
    if Path::new("/proc/driver/nvidia/gpus").exists() && run_nvidia_smi().is_some() {
        sources.push(Source::Nvidia);
    }
    sources.extend(amd_devices().into_iter().map(Source::Amd));
    sources
}

// ── NVIDIA ────────────────────────────────────────────────────────────────

fn collect_nvidia() -> Vec<GpuMetrics> {
    run_nvidia_smi()
        .map(|output| parse_nvidia_csv(&output))
        .unwrap_or_default()
}

fn run_nvidia_smi() -> Option<String> {
    run_bounded(
        "nvidia-smi",
        &[
            &format!("--query-gpu={NVIDIA_QUERY}"),
            "--format=csv,noheader,nounits",
        ],
        NVIDIA_TIMEOUT,
    )
}

/// Parse `nvidia-smi --format=csv,noheader,nounits` output for [`NVIDIA_QUERY`].
///
/// Unsupported figures are printed as `[N/A]` (and `[Not Supported]` on some
/// cards for power), which map to `None`. Memory columns are MiB.
pub fn parse_nvidia_csv(raw: &str) -> Vec<GpuMetrics> {
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(',').map(str::trim).collect();
            if fields.len() < 6 {
                return None;
            }
            let number = |value: &str| value.parse::<f64>().ok();
            Some(GpuMetrics {
                index: fields[0].parse().ok()?,
                vendor: "nvidia".into(),
                name: fields[1].to_string(),
                utilization_percent: number(fields[2]),
                memory_total: mib_to_bytes(number(fields[3])),
                memory_used: mib_to_bytes(number(fields[4])),
                temperature_c: number(fields[5]),
                power_watts: fields.get(6).copied().and_then(number),
            })
        })
        .collect()
}

fn mib_to_bytes(value: Option<f64>) -> u64 {
    value.map(|mib| (mib * 1024.0 * 1024.0) as u64).unwrap_or(0)
}

// ── AMD ───────────────────────────────────────────────────────────────────

/// `/sys/class/drm/cardN/device` directories that carry amdgpu's utilisation
/// counter. Render nodes (`renderD*`) and cards without the counter — old
/// radeon, virtual GPUs — are skipped.
fn amd_devices() -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir("/sys/class/drm") else {
        return Vec::new();
    };
    let mut devices: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_card_name)
        })
        .map(|path| path.join("device"))
        .filter(|device| device.join("gpu_busy_percent").exists())
        .collect();
    devices.sort();
    devices
}

/// `card0`, `card1`… but not `card0-DP-1` (a connector) or `renderD128`.
fn is_card_name(name: &str) -> bool {
    name.strip_prefix("card")
        .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
}

fn collect_amd(device: &Path) -> Option<GpuMetrics> {
    let index = device
        .parent()
        .and_then(|card| card.file_name())
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("card"))
        .and_then(|number| number.parse().ok())
        .unwrap_or(0);
    let read = |name: &str| read_number(&device.join(name));
    Some(GpuMetrics {
        index,
        vendor: "amd".into(),
        name: amd_name(device),
        utilization_percent: read("gpu_busy_percent"),
        memory_total: read("mem_info_vram_total").unwrap_or(0.0) as u64,
        memory_used: read("mem_info_vram_used").unwrap_or(0.0) as u64,
        // hwmon reports milli-degrees and micro-watts.
        temperature_c: amd_hwmon(device, "temp1_input").map(|value| value / 1000.0),
        power_watts: amd_hwmon(device, "power1_average")
            .or_else(|| amd_hwmon(device, "power1_input"))
            .map(|value| value / 1_000_000.0),
    })
}

/// A human-readable model. `product_name` exists only on recent kernels with
/// firmware that fills it in, so the PCI id from `uevent` is the fallback —
/// a bare "AMD GPU" would be indistinguishable between two cards.
fn amd_name(device: &Path) -> String {
    if let Some(name) = read_trimmed(&device.join("product_name")).filter(|n| !n.is_empty()) {
        return name;
    }
    match read_trimmed(&device.join("uevent")).and_then(|uevent| parse_pci_id(&uevent)) {
        Some(id) => format!("AMD GPU {id}"),
        None => "AMD GPU".into(),
    }
}

/// The `PCI_ID=1002:73BF` line of a device's `uevent`.
pub fn parse_pci_id(uevent: &str) -> Option<String> {
    uevent
        .lines()
        .find_map(|line| line.strip_prefix("PCI_ID="))
        .map(|id| id.trim().to_string())
}

/// One hwmon reading of an amdgpu card. The hwmon instance number is not
/// stable across boots, so the directory is scanned rather than guessed.
fn amd_hwmon(device: &Path, file: &str) -> Option<f64> {
    let entries = fs::read_dir(device.join("hwmon")).ok()?;
    entries
        .filter_map(Result::ok)
        .find_map(|entry| read_number(&entry.path().join(file)))
}

fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|raw| raw.trim().into())
}

fn read_number(path: &Path) -> Option<f64> {
    read_trimmed(path)?.parse().ok()
}

// ── process helper ────────────────────────────────────────────────────────

/// Run a command and read its stdout, giving up after `timeout`.
///
/// Output is read after the process exits, which is safe for the few hundred
/// bytes `nvidia-smi` prints but would deadlock on a command that fills the
/// 64 KiB pipe buffer — keep this for small, bounded output only.
fn run_bounded(program: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) => return None,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }

    let mut output = String::new();
    child.stdout.take()?.read_to_string(&mut output).ok()?;
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvidia_csv_parses_every_column() {
        let raw = "0, NVIDIA GeForce RTX 4090, 37, 24564, 1873, 52, 121.45\n";
        let gpus = parse_nvidia_csv(raw);
        assert_eq!(gpus.len(), 1);
        let gpu = &gpus[0];
        assert_eq!(gpu.index, 0);
        assert_eq!(gpu.vendor, "nvidia");
        assert_eq!(gpu.name, "NVIDIA GeForce RTX 4090");
        assert_eq!(gpu.utilization_percent, Some(37.0));
        assert_eq!(gpu.memory_total, 24564 * 1024 * 1024);
        assert_eq!(gpu.memory_used, 1873 * 1024 * 1024);
        assert_eq!(gpu.temperature_c, Some(52.0));
        assert_eq!(gpu.power_watts, Some(121.45));
    }

    #[test]
    fn nvidia_csv_handles_unsupported_fields() {
        let raw = "1, Tesla T4, [N/A], 15360, 0, 41, [Not Supported]";
        let gpu = &parse_nvidia_csv(raw)[0];
        assert_eq!(gpu.index, 1);
        assert_eq!(gpu.utilization_percent, None);
        assert_eq!(gpu.power_watts, None);
        assert_eq!(gpu.memory_total, 15360 * 1024 * 1024);
    }

    #[test]
    fn nvidia_csv_reads_several_cards_and_skips_junk() {
        let raw = "\
0, NVIDIA A100, 10, 40960, 100, 40, 60
truncated line
1, NVIDIA A100, 20, 40960, 200, 42, 61

";
        let gpus = parse_nvidia_csv(raw);
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[1].index, 1);
        assert_eq!(gpus[1].utilization_percent, Some(20.0));
    }

    #[test]
    fn card_names_exclude_connectors_and_render_nodes() {
        assert!(is_card_name("card0"));
        assert!(is_card_name("card12"));
        assert!(!is_card_name("card0-DP-1"));
        assert!(!is_card_name("renderD128"));
        assert!(!is_card_name("card"));
    }

    #[test]
    fn pci_id_comes_from_uevent() {
        let uevent = "DRIVER=amdgpu\nPCI_CLASS=30000\nPCI_ID=1002:73BF\nMODALIAS=pci:v1002\n";
        assert_eq!(parse_pci_id(uevent), Some("1002:73BF".into()));
        assert_eq!(parse_pci_id("DRIVER=amdgpu\n"), None);
    }

    #[test]
    fn missing_command_times_out_into_none() {
        assert!(run_bounded("asc-no-such-binary", &[], Duration::from_secs(1)).is_none());
    }

    // A machine without a GPU must return an empty list rather than failing,
    // which is the common case on the servers this daemon runs on.
    #[test]
    fn detector_never_fails() {
        let mut detector = Detector::new();
        let first = detector.collect();
        let second = detector.collect();
        assert_eq!(first.len(), second.len());
    }
}
