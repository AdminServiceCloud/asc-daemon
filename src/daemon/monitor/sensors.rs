//! Temperature and fan sensors from every device that exposes them.
//!
//! The kernel publishes sensors through `hwmon` (`/sys/class/hwmon/hwmonN`):
//! CPU packages and cores (`coretemp`, `k10temp`), GPUs (`amdgpu`), NVMe and
//! SATA drives (`nvme`, `drivetemp`), the motherboard's Super-I/O chip
//! (`nct6775`, `it87`), DIMM modules (`jc42`, `spd5118`) and some network
//! cards. Boards without hwmon (ARM single-board computers) usually have
//! `thermal_zone*` instead, which is the fallback.
//!
//! Each reading is attached to the device it belongs to — `kind` plus a
//! `device_id` that matches the hardware inventory (a GPU's PCI address, a
//! block device name) — so a UI can group "Samsung 990 PRO: 41 °C" instead of
//! showing a flat list of anonymous chips. NVIDIA GPUs have no hwmon node;
//! their temperature stays in `GpuMetrics`.
//!
//! A virtual machine normally has no sensors at all, which is a valid empty
//! result, not an error. Everything is read from sysfs; a sensor that fails to
//! read is skipped. Parsers take a root path so tests can build a fake tree.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// What kind of device a sensor sits on. The order is the display order.
pub const KINDS: &[&str] = &["cpu", "gpu", "disk", "board", "memory", "network", "other"];

/// One temperature reading.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SensorReading {
    /// One of [`KINDS`].
    pub kind: String,
    /// Identifies the device within its kind: `cpu`, a PCI address for GPUs,
    /// a block device or NVMe controller name for disks, an interface name
    /// for network cards.
    pub device_id: String,
    /// Kernel driver name of the sensor chip (`coretemp`, `nvme`…).
    pub chip: String,
    /// Sensor label (`Package id 0`, `Composite`) or `tempN` when the driver
    /// does not name it.
    pub label: String,
    pub temperature_c: f64,
    /// Driver-reported warning and critical thresholds, when it has them.
    pub max_c: Option<f64>,
    pub crit_c: Option<f64>,
}

/// One fan tachometer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FanReading {
    pub chip: String,
    pub label: String,
    pub rpm: u32,
}

/// Everything one scan found.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Sensors {
    pub temperatures: Vec<SensorReading>,
    pub fans: Vec<FanReading>,
}

/// Scan the live system (`/sys`).
pub fn collect() -> Sensors {
    scan(Path::new("/sys"))
}

/// Scan a sysfs tree rooted at `root` (tests pass a fake one).
pub fn scan(root: &Path) -> Sensors {
    let mut out = Sensors::default();
    let hwmon = root.join("class/hwmon");
    let mut dirs: Vec<PathBuf> = fs::read_dir(&hwmon)
        .map(|entries| entries.filter_map(Result::ok).map(|e| e.path()).collect())
        .unwrap_or_default();
    dirs.sort_by_key(|dir| numeric_suffix(dir, "hwmon"));

    for dir in &dirs {
        scan_hwmon(dir, &mut out);
    }
    if out.temperatures.is_empty() {
        scan_thermal_zones(root, &mut out);
    }
    out.temperatures.sort_by(|a, b| {
        kind_rank(&a.kind)
            .cmp(&kind_rank(&b.kind))
            .then_with(|| a.device_id.cmp(&b.device_id))
    });
    out
}

fn kind_rank(kind: &str) -> usize {
    KINDS.iter().position(|k| *k == kind).unwrap_or(KINDS.len())
}

fn scan_hwmon(dir: &Path, out: &mut Sensors) {
    let Some(chip) = read_trimmed(&dir.join("name")).filter(|name| !name.is_empty()) else {
        return;
    };
    let device = fs::canonicalize(dir.join("device")).ok();
    let (kind, device_id) = classify(&chip, device.as_deref());

    for index in indices(dir, "temp", "_input") {
        let Some(raw) = read_number(&dir.join(format!("temp{index}_input"))) else {
            continue;
        };
        // A header with nothing wired to it reads 0, or a large negative
        // sentinel; neither is a temperature.
        let Some(temperature_c) = plausible(raw) else {
            continue;
        };
        let threshold = |suffix: &str| {
            read_number(&dir.join(format!("temp{index}_{suffix}"))).and_then(plausible)
        };
        let label = read_trimmed(&dir.join(format!("temp{index}_label")))
            .filter(|label| !label.is_empty())
            .unwrap_or_else(|| format!("temp{index}"));
        out.temperatures.push(SensorReading {
            kind: kind.to_string(),
            device_id: device_id.clone(),
            chip: chip.clone(),
            label,
            temperature_c,
            max_c: threshold("max"),
            crit_c: threshold("crit"),
        });
    }

    for index in indices(dir, "fan", "_input") {
        let Some(rpm) = read_number(&dir.join(format!("fan{index}_input"))) else {
            continue;
        };
        let label = read_trimmed(&dir.join(format!("fan{index}_label")))
            .filter(|label| !label.is_empty())
            .unwrap_or_else(|| format!("fan{index}"));
        out.fans.push(FanReading {
            chip: chip.clone(),
            label,
            rpm: rpm.max(0.0) as u32,
        });
    }
}

/// Fallback for boards without hwmon: `/sys/class/thermal/thermal_zoneN`.
fn scan_thermal_zones(root: &Path, out: &mut Sensors) {
    let base = root.join("class/thermal");
    let mut zones: Vec<PathBuf> = fs::read_dir(&base)
        .map(|entries| entries.filter_map(Result::ok).map(|e| e.path()).collect())
        .unwrap_or_default();
    zones.retain(|zone| numeric_suffix(zone, "thermal_zone") != u32::MAX);
    zones.sort_by_key(|zone| numeric_suffix(zone, "thermal_zone"));
    for zone in zones {
        let Some(kind_name) = read_trimmed(&zone.join("type")) else {
            continue;
        };
        let Some(temperature_c) = read_number(&zone.join("temp")).and_then(plausible) else {
            continue;
        };
        let (kind, device_id) = match kind_name.as_str() {
            name if name.contains("cpu") || name.contains("soc") || name == "x86_pkg_temp" => {
                ("cpu", "cpu".to_string())
            }
            name if name.contains("gpu") => ("gpu", name.to_string()),
            "acpitz" => ("board", "board".to_string()),
            name => ("other", name.to_string()),
        };
        out.temperatures.push(SensorReading {
            kind: kind.to_string(),
            device_id,
            chip: "thermal_zone".into(),
            label: kind_name,
            temperature_c,
            max_c: None,
            crit_c: None,
        });
    }
}

/// Decide which device a hwmon chip belongs to, from its driver name and the
/// device node it hangs off.
pub fn classify(chip: &str, device: Option<&Path>) -> (&'static str, String) {
    let device_name = device
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .map(str::to_string);
    match chip {
        "coretemp" | "k10temp" | "k8temp" | "zenpower" | "cpu_thermal" | "cpu-thermal"
        | "fam15h_power" => ("cpu", "cpu".into()),
        "amdgpu" | "radeon" | "nouveau" | "i915" | "xe" => (
            "gpu",
            device_name
                .filter(|name| is_pci_address(name))
                .unwrap_or_else(|| chip.to_string()),
        ),
        "nvme" => ("disk", device_name.unwrap_or_else(|| chip.to_string())),
        "drivetemp" => (
            "disk",
            device
                .and_then(first_block_device)
                .unwrap_or_else(|| chip.to_string()),
        ),
        "jc42" | "spd5118" | "ee1004" => {
            ("memory", device_name.unwrap_or_else(|| chip.to_string()))
        }
        "acpitz" | "thinkpad" | "dell_smm" | "asus_wmi_sensors" | "gigabyte_wmi" => {
            ("board", "board".into())
        }
        name if name.starts_with("nct")
            || name.starts_with("it87")
            || name.starts_with("w83")
            || name.starts_with("f71")
            || name.starts_with("pch_")
            || name.starts_with("asus") =>
        {
            ("board", "board".into())
        }
        name if NETWORK_DRIVERS
            .iter()
            .any(|prefix| name.starts_with(prefix)) =>
        {
            (
                "network",
                device
                    .and_then(first_net_interface)
                    .unwrap_or_else(|| chip.to_string()),
            )
        }
        _ => ("other", chip.to_string()),
    }
}

/// Drivers of network cards that publish a temperature through hwmon.
const NETWORK_DRIVERS: &[&str] = &[
    "mlx", "ixgbe", "i40e", "ice", "bnxt", "atlantic", "r8169", "iwlwifi", "mt79", "ath1", "cxgb",
];

/// `0000:01:00.0` — domain:bus:device.function.
pub fn is_pci_address(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == 12
        && bytes[4] == b':'
        && bytes[7] == b':'
        && bytes[10] == b'.'
        && name
            .chars()
            .enumerate()
            .all(|(i, c)| matches!(i, 4 | 7 | 10) || c.is_ascii_hexdigit())
}

fn first_block_device(device: &Path) -> Option<String> {
    first_entry(&device.join("block"))
}

fn first_net_interface(device: &Path) -> Option<String> {
    first_entry(&device.join("net"))
}

fn first_entry(dir: &Path) -> Option<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    names.sort();
    names.into_iter().next()
}

/// Millidegrees → degrees, rejecting readings no sensor would report.
fn plausible(milli: f64) -> Option<f64> {
    if milli == 0.0 || !(-100_000.0..=250_000.0).contains(&milli) {
        return None;
    }
    Some(milli / 1000.0)
}

/// `tempN_input` indices present in a hwmon directory, ascending.
fn indices(dir: &Path, prefix: &str, suffix: &str) -> Vec<u32> {
    let mut found: Vec<u32> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter_map(|entry| {
                    let name = entry.file_name().into_string().ok()?;
                    name.strip_prefix(prefix)?
                        .strip_suffix(suffix)?
                        .parse()
                        .ok()
                })
                .collect()
        })
        .unwrap_or_default();
    found.sort_unstable();
    found
}

/// `hwmon12` → 12; a name without the prefix or digits sorts last.
fn numeric_suffix(path: &Path, prefix: &str) -> u32 {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(prefix))
        .and_then(|rest| rest.parse().ok())
        .unwrap_or(u32::MAX)
}

fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|raw| raw.trim().into())
}

fn read_number(path: &Path) -> Option<f64> {
    read_trimmed(path)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, relative: &str, content: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn groups_readings_by_device() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        write(root, "class/hwmon/hwmon0/name", "coretemp\n");
        write(root, "class/hwmon/hwmon0/temp1_input", "52000\n");
        write(root, "class/hwmon/hwmon0/temp1_label", "Package id 0\n");
        write(root, "class/hwmon/hwmon0/temp1_max", "100000\n");
        write(root, "class/hwmon/hwmon0/temp1_crit", "105000\n");
        write(root, "class/hwmon/hwmon0/temp2_input", "49000\n");
        write(root, "class/hwmon/hwmon0/temp2_label", "Core 0\n");

        // The device directory name is what ties a GPU to its PCI address.
        write(root, "devices/pci/0000:03:00.0/placeholder", "");
        write(root, "class/hwmon/hwmon1/name", "amdgpu\n");
        write(root, "class/hwmon/hwmon1/temp1_input", "61000\n");
        write(root, "class/hwmon/hwmon1/temp1_label", "edge\n");
        write(root, "class/hwmon/hwmon1/fan1_input", "1450\n");
        std::os::unix::fs::symlink(
            root.join("devices/pci/0000:03:00.0"),
            root.join("class/hwmon/hwmon1/device"),
        )
        .unwrap();

        write(root, "devices/nvme/nvme0/placeholder", "");
        write(root, "class/hwmon/hwmon2/name", "nvme\n");
        write(root, "class/hwmon/hwmon2/temp1_input", "38000\n");
        write(root, "class/hwmon/hwmon2/temp1_label", "Composite\n");
        std::os::unix::fs::symlink(
            root.join("devices/nvme/nvme0"),
            root.join("class/hwmon/hwmon2/device"),
        )
        .unwrap();

        write(root, "class/hwmon/hwmon3/name", "nct6798\n");
        write(root, "class/hwmon/hwmon3/temp1_input", "33000\n");
        write(root, "class/hwmon/hwmon3/fan2_input", "870\n");
        write(root, "class/hwmon/hwmon3/fan2_label", "CPU Fan\n");

        let sensors = scan(root);
        let kinds: Vec<(&str, &str, &str)> = sensors
            .temperatures
            .iter()
            .map(|t| (t.kind.as_str(), t.device_id.as_str(), t.label.as_str()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("cpu", "cpu", "Package id 0"),
                ("cpu", "cpu", "Core 0"),
                ("gpu", "0000:03:00.0", "edge"),
                ("disk", "nvme0", "Composite"),
                ("board", "board", "temp1"),
            ]
        );
        let package = &sensors.temperatures[0];
        assert_eq!(package.temperature_c, 52.0);
        assert_eq!(package.max_c, Some(100.0));
        assert_eq!(package.crit_c, Some(105.0));
        assert_eq!(sensors.temperatures[1].max_c, None);

        assert_eq!(sensors.fans.len(), 2);
        assert_eq!(sensors.fans[0].label, "fan1");
        assert_eq!(sensors.fans[0].rpm, 1450);
        assert_eq!(sensors.fans[1].label, "CPU Fan");
    }

    #[test]
    fn drops_unwired_channels() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "class/hwmon/hwmon0/name", "nct6798\n");
        write(root, "class/hwmon/hwmon0/temp1_input", "0\n");
        write(root, "class/hwmon/hwmon0/temp2_input", "-128000\n");
        write(root, "class/hwmon/hwmon0/temp3_input", "not a number\n");
        write(root, "class/hwmon/hwmon0/temp4_input", "36500\n");
        let sensors = scan(root);
        assert_eq!(sensors.temperatures.len(), 1);
        assert_eq!(sensors.temperatures[0].label, "temp4");
        assert_eq!(sensors.temperatures[0].temperature_c, 36.5);
    }

    #[test]
    fn hwmon_order_is_numeric() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for n in [10, 2, 1] {
            write(root, &format!("class/hwmon/hwmon{n}/name"), "acpitz\n");
            write(
                root,
                &format!("class/hwmon/hwmon{n}/temp1_input"),
                &format!("{}000\n", 20 + n),
            );
        }
        let temps: Vec<f64> = scan(root)
            .temperatures
            .iter()
            .map(|t| t.temperature_c)
            .collect();
        assert_eq!(temps, vec![21.0, 22.0, 30.0]);
    }

    #[test]
    fn falls_back_to_thermal_zones() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "class/thermal/thermal_zone0/type", "cpu-thermal\n");
        write(root, "class/thermal/thermal_zone0/temp", "47500\n");
        write(root, "class/thermal/thermal_zone1/type", "acpitz\n");
        write(root, "class/thermal/thermal_zone1/temp", "30000\n");
        write(root, "class/thermal/cooling_device0/type", "Processor\n");
        let sensors = scan(root);
        assert_eq!(sensors.temperatures.len(), 2);
        assert_eq!(sensors.temperatures[0].kind, "cpu");
        assert_eq!(sensors.temperatures[0].temperature_c, 47.5);
        assert_eq!(sensors.temperatures[1].kind, "board");
    }

    #[test]
    fn empty_tree_is_empty_result() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(scan(dir.path()), Sensors::default());
    }

    #[test]
    fn pci_addresses_are_recognised() {
        assert!(is_pci_address("0000:03:00.0"));
        assert!(is_pci_address("0000:0a:1f.7"));
        assert!(!is_pci_address("nvme0"));
        assert!(!is_pci_address("0-0018"));
        assert!(!is_pci_address("0000:03:00_0"));
    }

    #[test]
    fn classifies_known_chips() {
        assert_eq!(classify("k10temp", None), ("cpu", "cpu".to_string()));
        assert_eq!(classify("acpitz", None).0, "board");
        assert_eq!(classify("nct6775", None).0, "board");
        assert_eq!(classify("jc42", None).0, "memory");
        assert_eq!(classify("mlx5", None).0, "network");
        assert_eq!(classify("mystery", None), ("other", "mystery".to_string()));
    }
}
