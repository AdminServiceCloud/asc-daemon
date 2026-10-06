//! Hardware inventory of the node: what kind of machine this is and
//! what it is made of — as opposed to [`super::system`], which samples how busy
//! it is right now.
//!
//! Everything comes from procfs/sysfs, plus two optional helpers that are
//! already used elsewhere (`systemd-detect-virt`, `nvidia-smi`). No new
//! dependency, and no serial numbers: the inventory is meant to be shown in a
//! UI and shared with a support engineer, not to fingerprint the machine.
//!
//! Hardware barely changes while the daemon runs, and gathering it spawns
//! processes and walks sysfs, so the result is cached for [`CACHE_TTL`]; a
//! caller that wants fresh data (a card was hot-plugged) passes `refresh`.
//!
//! A virtual machine often hides most of this (no DMI, no SMBIOS memory
//! table). Missing pieces stay `None` / empty instead of being faked, and the
//! UI says the provider does not expose them.
//!
//! Parsers are pure functions over text or bytes, and the sysfs walkers take a
//! root path, so the whole module is testable without the hardware.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::gpu::run_bounded;

/// How long a gathered inventory is reused.
const CACHE_TTL: Duration = Duration::from_secs(600);

/// How long `systemd-detect-virt` / `nvidia-smi` may take.
const HELPER_TIMEOUT: Duration = Duration::from_secs(5);

static CACHE: Mutex<Option<(Instant, HardwareInfo)>> = Mutex::new(None);

/// The whole inventory.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HardwareInfo {
    pub machine: MachineInfo,
    pub cpu: CpuInfo,
    pub board: BoardInfo,
    pub memory: MemoryInfo,
    pub disks: Vec<DiskInfo>,
    pub gpus: Vec<GpuInfo>,
}

/// What the machine is: metal, a virtual machine or a container.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MachineInfo {
    /// `bare_metal`, `virtual_machine`, `container` or `unknown`.
    pub machine_type: String,
    /// `none`, `kvm`, `qemu`, `vmware`, `hyperv`, `xen`, `virtualbox`,
    /// `openvz`, `lxc`, `docker`, `wsl`… — the value `systemd-detect-virt`
    /// would print, with DMI as the fallback.
    pub virtualization: String,
    /// Hypervisor or cloud name recognised from DMI (`DigitalOcean`, `VMware`).
    pub hypervisor_vendor: Option<String>,
    pub system_vendor: Option<String>,
    pub product_name: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CpuInfo {
    pub vendor: Option<String>,
    pub model: Option<String>,
    pub sockets: u32,
    pub physical_cores: u32,
    /// Logical CPUs.
    pub threads: u32,
    pub max_mhz: Option<f64>,
}

/// The motherboard and its firmware, from DMI.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BoardInfo {
    pub vendor: Option<String>,
    pub name: Option<String>,
    pub version: Option<String>,
    pub bios_vendor: Option<String>,
    pub bios_version: Option<String>,
    pub bios_date: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MemoryInfo {
    pub total_bytes: u64,
    /// Populated DIMM slots; empty when SMBIOS is hidden (most VPS).
    pub modules: Vec<MemoryModule>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MemoryModule {
    pub slot: Option<String>,
    pub size_bytes: u64,
    /// `DDR4`, `DDR5`, `LPDDR5`…
    pub kind: Option<String>,
    /// Rated speed, MT/s.
    pub speed_mts: Option<u32>,
    /// Speed the module actually runs at, MT/s.
    pub configured_speed_mts: Option<u32>,
    pub manufacturer: Option<String>,
    pub part_number: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DiskInfo {
    /// Kernel block device name (`nvme0n1`, `sda`).
    pub name: String,
    pub model: Option<String>,
    pub vendor: Option<String>,
    pub size_bytes: u64,
    /// `nvme`, `ssd`, `hdd`, `virtual` or `unknown`.
    pub kind: String,
    /// `nvme`, `sata`, `scsi`, `usb`, `virtio`, `mmc` or `unknown`.
    pub transport: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GpuInfo {
    /// PCI address (`0000:01:00.0`): stable across reboots and between the
    /// inventory, sensors and the app's `$gpus` setting — unlike an index.
    pub id: String,
    /// `nvidia`, `amd`, `intel` or `other`.
    pub vendor: String,
    /// `10de:2204`.
    pub pci_id: String,
    pub model: String,
    pub vram_bytes: Option<u64>,
    pub driver: Option<String>,
    /// NVIDIA only: what `--gpus device=<uuid>` addresses.
    pub uuid: Option<String>,
    /// `/dev/dri/renderD128`.
    pub render_node: Option<String>,
    /// `/dev/dri/card0`.
    pub card_node: Option<String>,
    /// Whether the card can be handed to an app container on this host.
    pub attachable: bool,
    /// Why not, as a code the UI translates: `driver_not_loaded`,
    /// `toolkit_missing`, `no_render_node`, `unsupported_vendor`.
    pub attach_hint: Option<String>,
}

/// Gather the inventory, or return the cached one. Blocking.
pub fn hardware_info(refresh: bool) -> HardwareInfo {
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !refresh
        && let Some((at, info)) = cache.as_ref()
        && at.elapsed() < CACHE_TTL
    {
        return info.clone();
    }
    let info = gather();
    *cache = Some((Instant::now(), info.clone()));
    info
}

fn gather() -> HardwareInfo {
    let sys = Path::new("/sys");
    let cpuinfo = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();

    let board = read_board(sys);
    let mut cpu = parse_cpuinfo(&cpuinfo);
    if cpu.model.is_none() {
        cpu.model = read_trimmed(Path::new("/sys/firmware/devicetree/base/model"))
            .map(|model| model.trim_end_matches('\0').to_string());
    }
    if let Some(khz) = read_number(&sys.join("devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq")) {
        cpu.max_mhz = Some(khz / 1000.0);
    }

    let signals = VirtSignals {
        detect_vm: run_bounded("systemd-detect-virt", &["--vm"], HELPER_TIMEOUT),
        detect_container: run_bounded("systemd-detect-virt", &["--container"], HELPER_TIMEOUT),
        wsl: fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|release| {
                let release = release.to_ascii_lowercase();
                release.contains("microsoft") || release.contains("wsl")
            })
            .unwrap_or(false),
        container_env: container_from_environ(&fs::read("/proc/1/environ").unwrap_or_default()),
        dockerenv: Path::new("/.dockerenv").exists(),
        openvz: Path::new("/proc/vz").exists() && !Path::new("/proc/bc").exists(),
        hypervisor_flag: cpuinfo_has_hypervisor(&cpuinfo),
        sys_vendor: dmi(sys, "sys_vendor"),
        product_name: dmi(sys, "product_name"),
    };
    let machine = classify_machine(&signals);

    let memory = MemoryInfo {
        total_bytes: fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|raw| super::system::parse_meminfo(&raw))
            .map(|m| m.total)
            .unwrap_or(0),
        modules: fs::read(sys.join("firmware/dmi/tables/DMI"))
            .map(|table| parse_smbios_memory(&table))
            .unwrap_or_default(),
    };

    let nvidia = run_bounded(
        "nvidia-smi",
        &[
            "--query-gpu=pci.bus_id,uuid,name,memory.total",
            "--format=csv,noheader,nounits",
        ],
        HELPER_TIMEOUT,
    )
    .map(|raw| parse_nvidia_inventory(&raw))
    .unwrap_or_default();
    let pci_ids = ["/usr/share/misc/pci.ids", "/usr/share/hwdata/pci.ids"]
        .iter()
        .find_map(|path| fs::read_to_string(path).ok())
        .unwrap_or_default();
    let toolkit = nvidia_toolkit_installed();

    HardwareInfo {
        machine: MachineInfo {
            system_vendor: signals.sys_vendor.clone(),
            product_name: signals.product_name.clone(),
            ..machine
        },
        cpu,
        board,
        memory,
        disks: read_disks(sys),
        gpus: read_gpus(sys, Path::new("/dev"), &nvidia, &pci_ids, toolkit),
    }
}

// ── virtualization ────────────────────────────────────────────────────────

/// Everything that tells a VM or a container from metal. Gathered by
/// [`gather`], classified by the pure [`classify_machine`].
#[derive(Debug, Clone, Default)]
pub struct VirtSignals {
    /// Output of `systemd-detect-virt --vm`; `None` when it printed `none`
    /// (exit 1) or the tool is missing.
    pub detect_vm: Option<String>,
    pub detect_container: Option<String>,
    pub wsl: bool,
    /// `container=` from PID 1's environment (set by LXC, systemd-nspawn, podman).
    pub container_env: Option<String>,
    pub dockerenv: bool,
    pub openvz: bool,
    /// The `hypervisor` CPU flag.
    pub hypervisor_flag: bool,
    pub sys_vendor: Option<String>,
    pub product_name: Option<String>,
}

/// `(machine_type, virtualization, hypervisor_vendor)`.
pub fn classify_machine(signals: &VirtSignals) -> MachineInfo {
    let machine = |machine_type: &str, virtualization: &str, vendor: Option<&str>| MachineInfo {
        machine_type: machine_type.into(),
        virtualization: virtualization.into(),
        hypervisor_vendor: vendor.map(str::to_string),
        ..MachineInfo::default()
    };
    let named = |value: &Option<String>| {
        value
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty() && *v != "none")
            .map(str::to_string)
    };

    // WSL is a lightweight VM with a real Linux kernel, but systemd-detect-virt
    // files it under `--container`: it is a virtual machine for our purposes.
    let container = named(&signals.detect_container).or_else(|| named(&signals.container_env));
    if signals.wsl || container.as_deref() == Some("wsl") {
        return machine("virtual_machine", "wsl", None);
    }
    // A container shares the host's kernel, so the CPU flags and DMI it sees
    // are the host's: the container verdict has to come before them.
    if let Some(container) = container {
        return machine("container", &container, None);
    }
    if signals.openvz {
        return machine("container", "openvz", None);
    }
    if signals.dockerenv {
        return machine("container", "docker", None);
    }

    let dmi = dmi_hypervisor(
        signals.sys_vendor.as_deref().unwrap_or_default(),
        signals.product_name.as_deref().unwrap_or_default(),
    );
    if let Some(vm) = named(&signals.detect_vm) {
        return machine(
            "virtual_machine",
            &vm,
            dmi.as_ref().map(|(_, label)| *label),
        );
    }
    if let Some((virtualization, label)) = dmi {
        return machine("virtual_machine", virtualization, Some(label));
    }
    if signals.hypervisor_flag {
        return machine("virtual_machine", "unknown", None);
    }
    machine("bare_metal", "none", None)
}

/// Recognise a hypervisor or a cloud from DMI strings:
/// `(virtualization, display name)`.
pub fn dmi_hypervisor(sys_vendor: &str, product: &str) -> Option<(&'static str, &'static str)> {
    let vendor = sys_vendor.to_ascii_lowercase();
    let product = product.to_ascii_lowercase();
    let has = |needle: &str| vendor.contains(needle) || product.contains(needle);

    if has("vmware") {
        Some(("vmware", "VMware"))
    } else if has("virtualbox") || vendor.contains("innotek") {
        Some(("virtualbox", "VirtualBox"))
    } else if vendor.contains("microsoft") && product.contains("virtual") {
        Some(("hyperv", "Hyper-V"))
    } else if has("amazon ec2") || vendor.contains("amazon") {
        Some(("amazon", "Amazon EC2"))
    } else if has("google compute engine") || vendor.contains("google") {
        Some(("google", "Google Compute Engine"))
    } else if has("digitalocean") {
        Some(("kvm", "DigitalOcean"))
    } else if has("hetzner") {
        Some(("kvm", "Hetzner"))
    } else if has("openstack") {
        Some(("kvm", "OpenStack"))
    } else if has("alibaba cloud") {
        Some(("kvm", "Alibaba Cloud"))
    } else if has("parallels") {
        Some(("parallels", "Parallels"))
    } else if has("bhyve") {
        Some(("bhyve", "bhyve"))
    } else if vendor.contains("xen") || product.contains("hvm domu") {
        Some(("xen", "Xen"))
    } else if has("qemu") || has("bochs") || product.contains("standard pc") {
        Some(("kvm", "QEMU/KVM"))
    } else if vendor.contains("red hat") && (product.contains("kvm") || product.contains("rhev")) {
        Some(("kvm", "Red Hat KVM"))
    } else {
        None
    }
}

/// `container=<name>` out of `/proc/1/environ` (NUL-separated).
pub fn container_from_environ(environ: &[u8]) -> Option<String> {
    environ
        .split(|byte| *byte == 0)
        .filter_map(|entry| std::str::from_utf8(entry).ok())
        .find_map(|entry| entry.strip_prefix("container="))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

fn cpuinfo_has_hypervisor(cpuinfo: &str) -> bool {
    cpuinfo
        .lines()
        .filter(|line| line.starts_with("flags"))
        .any(|line| line.split_whitespace().any(|flag| flag == "hypervisor"))
}

// ── CPU ───────────────────────────────────────────────────────────────────

/// Summarise `/proc/cpuinfo`: x86 prints one block per logical CPU with
/// `physical id` / `core id`; ARM kernels print neither, so the counts fall back
/// to the number of blocks.
pub fn parse_cpuinfo(raw: &str) -> CpuInfo {
    let mut cpu = CpuInfo::default();
    let mut threads = 0u32;
    let mut packages: BTreeSet<String> = BTreeSet::new();
    let mut cores: BTreeSet<(String, String)> = BTreeSet::new();
    let mut cores_per_package = 0u32;
    let mut mhz_fallback: Option<f64> = None;
    let mut arm_implementer: Option<String> = None;

    for block in raw.split("\n\n") {
        let mut physical = None;
        let mut core = None;
        let mut is_cpu = false;
        for line in block.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            match key {
                "processor" => is_cpu = true,
                "vendor_id" if cpu.vendor.is_none() => cpu.vendor = Some(cpu_vendor(value)),
                "model name" | "Model Name" if cpu.model.is_none() => {
                    cpu.model = Some(value.to_string())
                }
                "Hardware" if cpu.model.is_none() => cpu.model = Some(value.to_string()),
                "CPU implementer" if arm_implementer.is_none() => {
                    arm_implementer = Some(value.to_string())
                }
                "physical id" => physical = Some(value.to_string()),
                "core id" => core = Some(value.to_string()),
                "cpu cores" => cores_per_package = value.parse().unwrap_or(cores_per_package),
                "cpu MHz" if mhz_fallback.is_none() => mhz_fallback = value.parse().ok(),
                _ => {}
            }
        }
        if !is_cpu {
            continue;
        }
        threads += 1;
        if let Some(physical) = &physical {
            packages.insert(physical.clone());
        }
        if let (Some(physical), Some(core)) = (physical, core) {
            cores.insert((physical, core));
        }
    }

    if cpu.vendor.is_none() {
        cpu.vendor = arm_implementer.as_deref().and_then(arm_vendor);
    }
    cpu.threads = threads;
    cpu.sockets = if packages.is_empty() && threads > 0 {
        1
    } else {
        packages.len() as u32
    };
    cpu.physical_cores = if !cores.is_empty() {
        cores.len() as u32
    } else if cores_per_package > 0 {
        cores_per_package * cpu.sockets.max(1)
    } else {
        threads
    };
    cpu.max_mhz = mhz_fallback;
    cpu
}

fn cpu_vendor(id: &str) -> String {
    match id {
        "GenuineIntel" => "Intel",
        "AuthenticAMD" => "AMD",
        "HygonGenuine" => "Hygon",
        "CentaurHauls" => "VIA/Zhaoxin",
        "GenuineTMx86" => "Transmeta",
        other => return other.to_string(),
    }
    .to_string()
}

/// ARM `CPU implementer` codes (the part of MIDR that names the licensee).
fn arm_vendor(implementer: &str) -> Option<String> {
    let name = match implementer.to_ascii_lowercase().as_str() {
        "0x41" => "ARM",
        "0x42" => "Broadcom",
        "0x43" => "Cavium",
        "0x46" => "Fujitsu",
        "0x48" => "HiSilicon",
        "0x4e" => "NVIDIA",
        "0x50" => "Ampere",
        "0x51" => "Qualcomm",
        "0x61" => "Apple",
        _ => return None,
    };
    Some(name.to_string())
}

// ── board and firmware ────────────────────────────────────────────────────

fn read_board(sys: &Path) -> BoardInfo {
    BoardInfo {
        vendor: dmi(sys, "board_vendor"),
        name: dmi(sys, "board_name"),
        version: dmi(sys, "board_version"),
        bios_vendor: dmi(sys, "bios_vendor"),
        bios_version: dmi(sys, "bios_version"),
        bios_date: dmi(sys, "bios_date"),
    }
}

/// One DMI string, `None` when absent or one of the placeholders vendors
/// leave in unfilled fields.
fn dmi(sys: &Path, field: &str) -> Option<String> {
    let value = read_trimmed(&sys.join("class/dmi/id").join(field))?;
    meaningful(&value)
}

/// Filter the "not filled in" boilerplate firmware ships.
pub fn meaningful(value: &str) -> Option<String> {
    let value = value.trim();
    let lower = value.to_ascii_lowercase();
    let placeholder = value.is_empty()
        || matches!(
            lower.as_str(),
            "not specified"
                | "unknown"
                | "to be filled by o.e.m."
                | "default string"
                | "system manufacturer"
                | "system product name"
                | "not applicable"
                | "n/a"
                | "none"
                | "no dimm"
                | "[empty]"
                | "0000"
        );
    (!placeholder).then(|| value.to_string())
}

// ── memory modules (SMBIOS type 17) ───────────────────────────────────────

/// Walk the SMBIOS structure table and return every populated "Memory Device".
///
/// Layout: a 4-byte header (type, length, handle), `length - 4` bytes of
/// formatted data, then NUL-terminated strings closed by an extra NUL. The
/// table ends at type 127. Offsets are from the SMBIOS 3.x specification.
pub fn parse_smbios_memory(table: &[u8]) -> Vec<MemoryModule> {
    let mut modules = Vec::new();
    let mut at = 0usize;
    while at + 4 <= table.len() {
        let kind = table[at];
        let length = table[at + 1] as usize;
        if length < 4 || at + length > table.len() {
            break;
        }
        let formatted = &table[at..at + length];

        // The string set: ends at the first pair of NULs.
        let strings_start = at + length;
        let mut end = strings_start;
        loop {
            if end + 1 >= table.len() {
                end = table.len();
                break;
            }
            if table[end] == 0 && table[end + 1] == 0 {
                end += 2;
                break;
            }
            end += 1;
        }
        if kind == 127 {
            break;
        }
        if kind == 17 {
            let strings: Vec<String> = table[strings_start..end.min(table.len())]
                .split(|byte| *byte == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).trim().to_string())
                .collect();
            if let Some(module) = parse_memory_device(formatted, &strings) {
                modules.push(module);
            }
        }
        at = end;
    }
    modules
}

fn parse_memory_device(data: &[u8], strings: &[String]) -> Option<MemoryModule> {
    if data.len() < 0x15 {
        return None;
    }
    let u16_at = |offset: usize| -> Option<u16> {
        data.get(offset..offset + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
    };
    let string = |offset: usize| -> Option<String> {
        let index = *data.get(offset)? as usize;
        if index == 0 {
            return None;
        }
        meaningful(strings.get(index - 1)?)
    };

    // 0 = empty slot, 0xFFFF = size unknown, 0x7FFF = see the extended field;
    // bit 15 switches the unit from MiB to KiB.
    let raw_size = u16_at(0x0C)?;
    let size_bytes = match raw_size {
        0 | 0xFFFF => return None,
        0x7FFF => {
            let extended = data.get(0x1C..0x20)?;
            u64::from(u32::from_le_bytes([
                extended[0],
                extended[1],
                extended[2],
                extended[3],
            ])) * 1024
                * 1024
        }
        size if size & 0x8000 != 0 => u64::from(size & 0x7FFF) * 1024,
        size => u64::from(size) * 1024 * 1024,
    };

    let speed = |offset: usize| u16_at(offset).filter(|s| *s != 0 && *s != 0xFFFF);
    Some(MemoryModule {
        slot: string(0x10),
        size_bytes,
        kind: data.get(0x12).and_then(|t| memory_type(*t)),
        speed_mts: speed(0x15).map(u32::from),
        configured_speed_mts: speed(0x20).map(u32::from),
        manufacturer: string(0x17),
        part_number: string(0x1A),
    })
}

fn memory_type(code: u8) -> Option<String> {
    let name = match code {
        0x0F => "SDRAM",
        0x12 => "DDR",
        0x13 => "DDR2",
        0x14 => "DDR2 FB-DIMM",
        0x18 => "DDR3",
        0x1A => "DDR4",
        0x1B => "LPDDR",
        0x1C => "LPDDR2",
        0x1D => "LPDDR3",
        0x1E => "LPDDR4",
        0x20 => "HBM",
        0x21 => "HBM2",
        0x22 => "DDR5",
        0x23 => "LPDDR5",
        0x24 => "HBM3",
        _ => return None,
    };
    Some(name.to_string())
}

// ── disks ─────────────────────────────────────────────────────────────────

/// Physical (and virtual-disk) block devices. Loop, RAM, zram, device-mapper,
/// software RAID and optical drives are not hardware.
fn read_disks(sys: &Path) -> Vec<DiskInfo> {
    let Ok(entries) = fs::read_dir(sys.join("block")) else {
        return Vec::new();
    };
    let mut disks: Vec<DiskInfo> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if ["loop", "ram", "zram", "dm-", "md", "sr", "fd", "nbd"]
                .iter()
                .any(|prefix| name.starts_with(prefix))
            {
                return None;
            }
            let dir = entry.path();
            let sectors = read_number(&dir.join("size"))? as u64;
            if sectors == 0 {
                return None;
            }
            let rotational = read_trimmed(&dir.join("queue/rotational")).map(|v| v == "1");
            let device_link = fs::canonicalize(dir.join("device"))
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            let transport = disk_transport(&name, &device_link);
            let model = read_trimmed(&dir.join("device/model")).and_then(|m| meaningful(&m));
            let kind = if model.as_deref().is_some_and(looks_virtual) {
                "virtual".to_string()
            } else {
                disk_kind(&transport, rotational)
            };
            Some(DiskInfo {
                kind,
                model,
                vendor: read_trimmed(&dir.join("device/vendor")).and_then(|v| meaningful(&v)),
                size_bytes: sectors * 512,
                transport,
                name,
            })
        })
        .collect();
    disks.sort_by(|a, b| a.name.cmp(&b.name));
    disks
}

/// Which bus the disk sits on, from its name and the sysfs path of its device.
pub fn disk_transport(name: &str, device_path: &str) -> String {
    let transport = if name.starts_with("nvme") {
        "nvme"
    } else if name.starts_with("mmcblk") {
        "mmc"
    } else if name.starts_with("vd") || name.starts_with("xvd") || device_path.contains("/virtio") {
        "virtio"
    } else if device_path.contains("/usb") {
        "usb"
    } else if device_path.contains("/ata") {
        "sata"
    } else if name.starts_with("sd") {
        "scsi"
    } else {
        "unknown"
    };
    transport.to_string()
}

/// Hypervisors name their emulated disks after themselves ("Virtual Disk" on
/// Hyper-V and WSL, "QEMU HARDDISK", "VMware Virtual S"…) and report them as
/// rotational whatever they sit on.
pub fn looks_virtual(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    ["virtual", "qemu", "vmware", "vbox", "xen", "msft"]
        .iter()
        .any(|marker| model.contains(marker))
}

pub fn disk_kind(transport: &str, rotational: Option<bool>) -> String {
    // `rotational` is a guess the hypervisor makes up for virtual disks.
    let kind = match (transport, rotational) {
        ("virtio", _) => "virtual",
        ("nvme", _) => "nvme",
        (_, Some(true)) => "hdd",
        (_, Some(false)) => "ssd",
        _ => "unknown",
    };
    kind.to_string()
}

// ── GPUs ──────────────────────────────────────────────────────────────────

/// One row of `nvidia-smi --query-gpu=pci.bus_id,uuid,name,memory.total`.
#[derive(Debug, Clone, PartialEq)]
pub struct NvidiaCard {
    /// Normalised to sysfs form: `0000:01:00.0`.
    pub pci_address: String,
    pub uuid: String,
    pub name: String,
    pub memory_bytes: Option<u64>,
}

pub fn parse_nvidia_inventory(raw: &str) -> Vec<NvidiaCard> {
    raw.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(',').map(str::trim).collect();
            if fields.len() < 4 {
                return None;
            }
            Some(NvidiaCard {
                pci_address: normalize_pci_address(fields[0]),
                uuid: fields[1].to_string(),
                name: fields[2].to_string(),
                memory_bytes: fields[3]
                    .parse::<f64>()
                    .ok()
                    .map(|mib| (mib * 1024.0 * 1024.0) as u64),
            })
        })
        .collect()
}

/// `nvidia-smi` prints an 8-digit domain (`00000000:01:00.0`); sysfs uses 4.
pub fn normalize_pci_address(address: &str) -> String {
    let address = address.trim().to_ascii_lowercase();
    match address.split_once(':') {
        Some((domain, rest)) if domain.len() > 4 => {
            format!("{}:{rest}", &domain[domain.len() - 4..])
        }
        _ => address,
    }
}

/// Vendor and device names out of the system's `pci.ids` database.
pub fn pci_ids_lookup(db: &str, vendor: &str, device: &str) -> Option<String> {
    let mut in_vendor = false;
    for line in db.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        if !line.starts_with('\t') {
            if in_vendor {
                return None;
            }
            in_vendor = line
                .get(..4)
                .is_some_and(|id| id.eq_ignore_ascii_case(vendor));
        } else if in_vendor && !line.starts_with("\t\t") {
            let entry = &line[1..];
            if entry
                .get(..4)
                .is_some_and(|id| id.eq_ignore_ascii_case(device))
            {
                return Some(entry[4..].trim().to_string());
            }
        }
    }
    None
}

fn read_gpus(
    sys: &Path,
    dev: &Path,
    nvidia: &[NvidiaCard],
    pci_ids: &str,
    toolkit: bool,
) -> Vec<GpuInfo> {
    let Ok(entries) = fs::read_dir(sys.join("bus/pci/devices")) else {
        return Vec::new();
    };
    let mut gpus: Vec<GpuInfo> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let address = entry.file_name().into_string().ok()?;
            let dir = entry.path();
            // PCI class 0x03: display controllers (VGA, 3D, XGA).
            let class = read_trimmed(&dir.join("class"))?;
            if !class.trim_start_matches("0x").starts_with("03") {
                return None;
            }
            let vendor_id = read_trimmed(&dir.join("vendor"))?
                .trim_start_matches("0x")
                .to_ascii_lowercase();
            let device_id = read_trimmed(&dir.join("device"))
                .unwrap_or_default()
                .trim_start_matches("0x")
                .to_ascii_lowercase();
            let vendor = match vendor_id.as_str() {
                "10de" => "nvidia",
                "1002" => "amd",
                "8086" => "intel",
                _ => "other",
            };
            let driver = fs::read_link(dir.join("driver"))
                .ok()
                .and_then(|link| link.file_name().map(|n| n.to_string_lossy().into_owned()));

            let card = nvidia.iter().find(|card| card.pci_address == address);
            let model = card
                .map(|card| card.name.clone())
                .or_else(|| read_trimmed(&dir.join("product_name")).and_then(|n| meaningful(&n)))
                .or_else(|| pci_ids_lookup(pci_ids, &vendor_id, &device_id))
                .unwrap_or_else(|| format!("GPU {vendor_id}:{device_id}"));
            let vram_bytes = card
                .and_then(|card| card.memory_bytes)
                .or_else(|| read_number(&dir.join("mem_info_vram_total")).map(|v| v as u64));

            let (render_node, card_node) = drm_nodes(&dir.join("drm"), dev);
            let (attachable, attach_hint) =
                attach_status(vendor, driver.as_deref(), toolkit, render_node.is_some());
            Some(GpuInfo {
                id: address,
                vendor: vendor.to_string(),
                pci_id: format!("{vendor_id}:{device_id}"),
                model,
                vram_bytes,
                driver,
                uuid: card.map(|card| card.uuid.clone()),
                render_node,
                card_node,
                attachable,
                attach_hint,
            })
        })
        .collect();
    gpus.sort_by(|a, b| a.id.cmp(&b.id));
    gpus
}

/// `(renderD*, card*)` device nodes of a GPU, as `/dev/dri/...` paths, only
/// when the node really exists in `dev`.
fn drm_nodes(drm_dir: &Path, dev: &Path) -> (Option<String>, Option<String>) {
    let mut render = None;
    let mut card = None;
    let mut names: Vec<String> = fs::read_dir(drm_dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    for name in names {
        let path = dev.join("dri").join(&name);
        if !path.exists() {
            continue;
        }
        let shown = format!("/dev/dri/{name}");
        if name.starts_with("renderD") && render.is_none() {
            render = Some(shown);
        } else if name.starts_with("card") && !name.contains('-') && card.is_none() {
            card = Some(shown);
        }
    }
    (render, card)
}

/// Whether an app container can be given this GPU, and if not, why (a code).
pub fn attach_status(
    vendor: &str,
    driver: Option<&str>,
    nvidia_toolkit: bool,
    has_render_node: bool,
) -> (bool, Option<String>) {
    let refuse = |code: &str| (false, Some(code.to_string()));
    match vendor {
        "nvidia" if driver != Some("nvidia") => refuse("driver_not_loaded"),
        "nvidia" if !nvidia_toolkit => refuse("toolkit_missing"),
        "nvidia" => (true, None),
        "amd" | "intel" if !has_render_node => refuse("no_render_node"),
        "amd" | "intel" => (true, None),
        _ => refuse("unsupported_vendor"),
    }
}

/// NVIDIA Container Toolkit hooks Docker through a binary on the host; if none
/// of its entry points exists, `--gpus` fails at container start.
fn nvidia_toolkit_installed() -> bool {
    const BINARIES: &[&str] = &[
        "nvidia-container-runtime-hook",
        "nvidia-container-toolkit",
        "nvidia-container-runtime",
        "nvidia-ctk",
    ];
    const DIRS: &[&str] = &["/usr/bin", "/usr/local/bin", "/usr/sbin", "/sbin", "/bin"];
    DIRS.iter().any(|dir| {
        BINARIES
            .iter()
            .any(|binary| PathBuf::from(dir).join(binary).exists())
    })
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

    const X86_CPUINFO: &str = "\
processor\t: 0
vendor_id\t: GenuineIntel
model name\t: Intel(R) Xeon(R) E-2288G CPU @ 3.70GHz
cpu MHz\t\t: 3700.000
physical id\t: 0
cpu cores\t: 2
core id\t\t: 0
flags\t\t: fpu vme de pse

processor\t: 1
vendor_id\t: GenuineIntel
model name\t: Intel(R) Xeon(R) E-2288G CPU @ 3.70GHz
physical id\t: 0
cpu cores\t: 2
core id\t\t: 1

processor\t: 2
vendor_id\t: GenuineIntel
model name\t: Intel(R) Xeon(R) E-2288G CPU @ 3.70GHz
physical id\t: 0
cpu cores\t: 2
core id\t\t: 0

processor\t: 3
vendor_id\t: GenuineIntel
model name\t: Intel(R) Xeon(R) E-2288G CPU @ 3.70GHz
physical id\t: 0
cpu cores\t: 2
core id\t\t: 1

";

    #[test]
    fn x86_cpu_counts_cores_threads_and_sockets() {
        let cpu = parse_cpuinfo(X86_CPUINFO);
        assert_eq!(cpu.vendor.as_deref(), Some("Intel"));
        assert_eq!(
            cpu.model.as_deref(),
            Some("Intel(R) Xeon(R) E-2288G CPU @ 3.70GHz")
        );
        assert_eq!(cpu.sockets, 1);
        assert_eq!(cpu.physical_cores, 2);
        assert_eq!(cpu.threads, 4);
        assert_eq!(cpu.max_mhz, Some(3700.0));
    }

    #[test]
    fn arm_cpu_has_no_topology_and_names_the_licensee() {
        let raw = "\
processor\t: 0
BogoMIPS\t: 108.00
CPU implementer\t: 0x41
CPU architecture: 8
CPU part\t: 0xd08

processor\t: 1
CPU implementer\t: 0x41

Hardware\t: BCM2711
";
        let cpu = parse_cpuinfo(raw);
        assert_eq!(cpu.vendor.as_deref(), Some("ARM"));
        assert_eq!(cpu.model.as_deref(), Some("BCM2711"));
        assert_eq!(cpu.threads, 2);
        assert_eq!(cpu.sockets, 1);
        assert_eq!(cpu.physical_cores, 2);
    }

    #[test]
    fn empty_cpuinfo_is_empty_cpu() {
        let cpu = parse_cpuinfo("");
        assert_eq!(cpu.threads, 0);
        assert_eq!(cpu.sockets, 0);
    }

    #[test]
    fn hypervisor_flag_is_found_in_flags_only() {
        assert!(cpuinfo_has_hypervisor(
            "flags\t: fpu vme hypervisor lahf_lm\n"
        ));
        assert!(!cpuinfo_has_hypervisor(
            "flags\t: fpu vme\nmodel name: hypervisor\n"
        ));
    }

    fn signals() -> VirtSignals {
        VirtSignals::default()
    }

    #[test]
    fn plain_machine_is_bare_metal() {
        let machine = classify_machine(&VirtSignals {
            sys_vendor: Some("ASUS".into()),
            product_name: Some("System Product Name".into()),
            ..signals()
        });
        assert_eq!(machine.machine_type, "bare_metal");
        assert_eq!(machine.virtualization, "none");
    }

    #[test]
    fn detect_virt_wins_and_dmi_names_the_cloud() {
        let machine = classify_machine(&VirtSignals {
            detect_vm: Some("kvm\n".into()),
            sys_vendor: Some("DigitalOcean".into()),
            product_name: Some("Droplet".into()),
            hypervisor_flag: true,
            ..signals()
        });
        assert_eq!(machine.machine_type, "virtual_machine");
        assert_eq!(machine.virtualization, "kvm");
        assert_eq!(machine.hypervisor_vendor.as_deref(), Some("DigitalOcean"));
    }

    #[test]
    fn dmi_alone_identifies_a_vm_without_systemd() {
        let machine = classify_machine(&VirtSignals {
            sys_vendor: Some("QEMU".into()),
            product_name: Some("Standard PC (Q35 + ICH9, 2009)".into()),
            ..signals()
        });
        assert_eq!(machine.machine_type, "virtual_machine");
        assert_eq!(machine.virtualization, "kvm");
        assert_eq!(machine.hypervisor_vendor.as_deref(), Some("QEMU/KVM"));
    }

    #[test]
    fn hypervisor_flag_without_dmi_is_an_unnamed_vm() {
        let machine = classify_machine(&VirtSignals {
            hypervisor_flag: true,
            ..signals()
        });
        assert_eq!(machine.machine_type, "virtual_machine");
        assert_eq!(machine.virtualization, "unknown");
    }

    #[test]
    fn containers_are_decided_before_cpu_flags_and_dmi() {
        let lxc = classify_machine(&VirtSignals {
            container_env: Some("lxc".into()),
            hypervisor_flag: true,
            sys_vendor: Some("VMware, Inc.".into()),
            ..signals()
        });
        assert_eq!(
            (lxc.machine_type.as_str(), lxc.virtualization.as_str()),
            ("container", "lxc")
        );

        let docker = classify_machine(&VirtSignals {
            dockerenv: true,
            ..signals()
        });
        assert_eq!(docker.virtualization, "docker");

        let openvz = classify_machine(&VirtSignals {
            openvz: true,
            ..signals()
        });
        assert_eq!(openvz.virtualization, "openvz");

        let none = classify_machine(&VirtSignals {
            detect_container: Some("none".into()),
            ..signals()
        });
        assert_eq!(none.machine_type, "bare_metal");
    }

    #[test]
    fn wsl_is_a_virtual_machine() {
        let machine = classify_machine(&VirtSignals {
            wsl: true,
            hypervisor_flag: true,
            ..signals()
        });
        assert_eq!(machine.machine_type, "virtual_machine");
        assert_eq!(machine.virtualization, "wsl");
    }

    #[test]
    fn systemd_files_wsl_under_containers_but_it_is_a_virtual_machine() {
        let machine = classify_machine(&VirtSignals {
            detect_container: Some(
                "wsl
"
                .into(),
            ),
            ..signals()
        });
        assert_eq!(machine.machine_type, "virtual_machine");
        assert_eq!(machine.virtualization, "wsl");
    }

    #[test]
    fn hypervisor_disk_names_are_recognised() {
        assert!(looks_virtual("Virtual Disk"));
        assert!(looks_virtual("QEMU HARDDISK"));
        assert!(looks_virtual("VMware Virtual S"));
        assert!(!looks_virtual("Samsung SSD 990 PRO 1TB"));
        assert!(!looks_virtual("ST4000DM004-2CV1"));
    }

    #[test]
    fn container_env_comes_from_pid1_environment() {
        let environ = b"PATH=/usr/bin\0container=lxc\0HOME=/root\0";
        assert_eq!(container_from_environ(environ).as_deref(), Some("lxc"));
        assert_eq!(container_from_environ(b"PATH=/usr/bin\0"), None);
    }

    #[test]
    fn dmi_recognises_common_hypervisors() {
        assert_eq!(
            dmi_hypervisor("VMware, Inc.", "VMware7,1").unwrap().0,
            "vmware"
        );
        assert_eq!(
            dmi_hypervisor("Microsoft Corporation", "Virtual Machine")
                .unwrap()
                .0,
            "hyperv"
        );
        assert_eq!(
            dmi_hypervisor("innotek GmbH", "VirtualBox").unwrap().0,
            "virtualbox"
        );
        assert_eq!(
            dmi_hypervisor("Amazon EC2", "t3.micro").unwrap().0,
            "amazon"
        );
        assert_eq!(dmi_hypervisor("Xen", "HVM domU").unwrap().0, "xen");
        assert!(dmi_hypervisor("Supermicro", "X11SCH-F").is_none());
        assert!(dmi_hypervisor("Microsoft Corporation", "Surface Laptop 4").is_none());
    }

    #[test]
    fn firmware_placeholders_are_dropped() {
        assert_eq!(meaningful("To Be Filled By O.E.M."), None);
        assert_eq!(meaningful("  "), None);
        assert_eq!(meaningful("Not Specified"), None);
        assert_eq!(
            meaningful("ASUSTeK COMPUTER INC.").as_deref(),
            Some("ASUSTeK COMPUTER INC.")
        );
    }

    /// A Memory Device record with the given fields and string set.
    fn memory_record(
        size: u16,
        kind: u8,
        speed: u16,
        configured: u16,
        strings: &[&str],
    ) -> Vec<u8> {
        let mut data = vec![0u8; 0x28];
        data[0] = 17;
        data[1] = 0x28;
        data[0x0C..0x0E].copy_from_slice(&size.to_le_bytes());
        // 0x7FFF is "see the extended size", in MiB; 32 GiB does not fit the
        // 15-bit field.
        data[0x1C..0x20].copy_from_slice(&32768u32.to_le_bytes());
        data[0x10] = 1; // device locator
        data[0x12] = kind;
        data[0x15..0x17].copy_from_slice(&speed.to_le_bytes());
        data[0x17] = 2; // manufacturer
        data[0x1A] = 3; // part number
        data[0x20..0x22].copy_from_slice(&configured.to_le_bytes());
        for s in strings {
            data.extend_from_slice(s.as_bytes());
            data.push(0);
        }
        if strings.is_empty() {
            data.push(0);
        }
        data.push(0);
        data
    }

    #[test]
    fn smbios_table_yields_populated_dimms_only() {
        let mut table = Vec::new();
        // A non-memory structure first (BIOS information, type 0) to prove
        // the walker steps over unrelated records.
        table.extend_from_slice(&[0, 4, 0, 0]);
        table.extend_from_slice(b"Vendor\0\0");
        table.extend(memory_record(
            16384,
            0x1A,
            3200,
            2933,
            &["DIMM_A1", "Samsung", "M378A2K43DB1-CTD"],
        ));
        // Empty slot.
        table.extend(memory_record(
            0,
            0x1A,
            0,
            0,
            &["DIMM_A2", "NO DIMM", "[Empty]"],
        ));
        table.extend(memory_record(
            0x7FFF,
            0x22,
            4800,
            4800,
            &["DIMM_B1", "Micron", "MTC20F"],
        ));
        table.extend_from_slice(&[127, 4, 0, 0, 0, 0]);
        // Anything after the terminator must be ignored.
        table.extend(memory_record(8192, 0x1A, 2400, 2400, &["X", "Y", "Z"]));

        let modules = parse_smbios_memory(&table);
        assert_eq!(modules.len(), 2);
        assert_eq!(modules[0].slot.as_deref(), Some("DIMM_A1"));
        assert_eq!(modules[0].size_bytes, 16 * 1024 * 1024 * 1024);
        assert_eq!(modules[0].kind.as_deref(), Some("DDR4"));
        assert_eq!(modules[0].speed_mts, Some(3200));
        assert_eq!(modules[0].configured_speed_mts, Some(2933));
        assert_eq!(modules[0].manufacturer.as_deref(), Some("Samsung"));
        assert_eq!(modules[0].part_number.as_deref(), Some("M378A2K43DB1-CTD"));
        assert_eq!(modules[1].kind.as_deref(), Some("DDR5"));
        assert_eq!(modules[1].size_bytes, 32 * 1024 * 1024 * 1024);
    }

    #[test]
    fn smbios_handles_kib_units_and_truncated_tables() {
        let table = memory_record(0x8000 | 512, 0x1A, 0xFFFF, 0, &["A", "B", "C"]);
        let modules = parse_smbios_memory(&table);
        assert_eq!(modules[0].size_bytes, 512 * 1024);
        assert_eq!(modules[0].speed_mts, None);

        let mut truncated = memory_record(1024, 0x1A, 2400, 2400, &["A", "B", "C"]);
        truncated.truncate(10);
        assert!(parse_smbios_memory(&truncated).is_empty());
        assert!(parse_smbios_memory(&[]).is_empty());
    }

    #[test]
    fn disks_skip_virtual_devices_and_classify_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "block/nvme0n1/size", "1953525168\n");
        write(root, "block/nvme0n1/queue/rotational", "0\n");
        write(
            root,
            "block/nvme0n1/device/model",
            "Samsung SSD 990 PRO 1TB\n",
        );
        write(root, "block/sda/size", "7814037168\n");
        write(root, "block/sda/queue/rotational", "1\n");
        write(root, "block/sda/device/model", "ST4000DM004-2CV1\n");
        write(root, "block/sda/device/vendor", "ATA\n");
        write(root, "block/vda/size", "209715200\n");
        write(root, "block/vda/queue/rotational", "1\n");
        write(root, "block/loop0/size", "2048\n");
        write(root, "block/zram0/size", "2048\n");
        write(root, "block/sr0/size", "2097151\n");
        write(root, "block/sdb/size", "0\n");

        let disks = read_disks(root);
        let summary: Vec<(&str, &str, &str)> = disks
            .iter()
            .map(|d| (d.name.as_str(), d.kind.as_str(), d.transport.as_str()))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("nvme0n1", "nvme", "nvme"),
                ("sda", "hdd", "scsi"),
                ("vda", "virtual", "virtio"),
            ]
        );
        assert_eq!(disks[0].size_bytes, 1953525168 * 512);
        assert_eq!(disks[0].model.as_deref(), Some("Samsung SSD 990 PRO 1TB"));
        assert_eq!(disks[1].vendor.as_deref(), Some("ATA"));
    }

    #[test]
    fn nvidia_inventory_normalises_the_pci_address() {
        let raw = "00000000:01:00.0, GPU-aaaa-bbbb, NVIDIA GeForce RTX 4090, 24564\n\
                   garbage\n\
                   00000000:02:00.0, GPU-cccc, NVIDIA GeForce RTX 3060, 12288\n";
        let cards = parse_nvidia_inventory(raw);
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].pci_address, "0000:01:00.0");
        assert_eq!(cards[0].uuid, "GPU-aaaa-bbbb");
        assert_eq!(cards[0].memory_bytes, Some(24564 * 1024 * 1024));
        assert_eq!(normalize_pci_address("0000:0A:00.0"), "0000:0a:00.0");
    }

    const PCI_IDS: &str = "\
# comment
1002  Advanced Micro Devices, Inc. [AMD/ATI]
\t73bf  Navi 21 [Radeon RX 6800/6800 XT / 6900 XT]
\t\t1002 0b36  Subsystem line
\t73df  Navi 22 [Radeon RX 6700 XT]
10de  NVIDIA Corporation
\t2204  GA102 [GeForce RTX 3090]
";

    #[test]
    fn pci_ids_lookup_finds_the_device_within_its_vendor() {
        assert_eq!(
            pci_ids_lookup(PCI_IDS, "1002", "73df").as_deref(),
            Some("Navi 22 [Radeon RX 6700 XT]")
        );
        assert_eq!(
            pci_ids_lookup(PCI_IDS, "10de", "2204").as_deref(),
            Some("GA102 [GeForce RTX 3090]")
        );
        // A device id that exists under another vendor must not match.
        assert_eq!(pci_ids_lookup(PCI_IDS, "10de", "73bf"), None);
        assert_eq!(pci_ids_lookup(PCI_IDS, "dead", "beef"), None);
    }

    #[test]
    fn gpus_come_from_pci_class_03_with_nodes_and_readiness() {
        let sys = tempfile::tempdir().unwrap();
        let dev = tempfile::tempdir().unwrap();
        let (root, devs) = (sys.path(), dev.path());

        // NVIDIA card with the driver bound.
        write(root, "bus/pci/devices/0000:01:00.0/class", "0x030000\n");
        write(root, "bus/pci/devices/0000:01:00.0/vendor", "0x10de\n");
        write(root, "bus/pci/devices/0000:01:00.0/device", "0x2204\n");
        write(root, "drivers/nvidia/placeholder", "");
        std::os::unix::fs::symlink(
            root.join("drivers/nvidia"),
            root.join("bus/pci/devices/0000:01:00.0/driver"),
        )
        .unwrap();

        // AMD card with DRM nodes.
        write(root, "bus/pci/devices/0000:03:00.0/class", "0x038000\n");
        write(root, "bus/pci/devices/0000:03:00.0/vendor", "0x1002\n");
        write(root, "bus/pci/devices/0000:03:00.0/device", "0x73df\n");
        write(
            root,
            "bus/pci/devices/0000:03:00.0/mem_info_vram_total",
            "12868124672\n",
        );
        write(
            root,
            "bus/pci/devices/0000:03:00.0/drm/card1/placeholder",
            "",
        );
        write(
            root,
            "bus/pci/devices/0000:03:00.0/drm/card1-DP-1/placeholder",
            "",
        );
        write(
            root,
            "bus/pci/devices/0000:03:00.0/drm/renderD128/placeholder",
            "",
        );
        write(devs, "dri/card1", "");
        write(devs, "dri/renderD128", "");

        // A network card must be ignored.
        write(root, "bus/pci/devices/0000:05:00.0/class", "0x020000\n");
        write(root, "bus/pci/devices/0000:05:00.0/vendor", "0x8086\n");

        let nvidia = vec![NvidiaCard {
            pci_address: "0000:01:00.0".into(),
            uuid: "GPU-1234".into(),
            name: "NVIDIA GeForce RTX 3090".into(),
            memory_bytes: Some(24 * 1024 * 1024 * 1024),
        }];

        let without_toolkit = read_gpus(root, devs, &nvidia, PCI_IDS, false);
        assert_eq!(without_toolkit.len(), 2);
        let nv = &without_toolkit[0];
        assert_eq!(nv.id, "0000:01:00.0");
        assert_eq!(nv.vendor, "nvidia");
        assert_eq!(nv.uuid.as_deref(), Some("GPU-1234"));
        assert_eq!(nv.driver.as_deref(), Some("nvidia"));
        assert!(!nv.attachable);
        assert_eq!(nv.attach_hint.as_deref(), Some("toolkit_missing"));

        let amd = &without_toolkit[1];
        assert_eq!(amd.vendor, "amd");
        assert_eq!(amd.model, "Navi 22 [Radeon RX 6700 XT]");
        assert_eq!(amd.vram_bytes, Some(12868124672));
        assert_eq!(amd.render_node.as_deref(), Some("/dev/dri/renderD128"));
        assert_eq!(amd.card_node.as_deref(), Some("/dev/dri/card1"));
        assert!(amd.attachable);

        let with_toolkit = read_gpus(root, devs, &nvidia, PCI_IDS, true);
        assert!(with_toolkit[0].attachable);
        assert_eq!(with_toolkit[0].attach_hint, None);
    }

    #[test]
    fn attach_readiness_per_vendor() {
        assert_eq!(
            attach_status("nvidia", Some("nouveau"), true, true)
                .1
                .as_deref(),
            Some("driver_not_loaded")
        );
        assert_eq!(
            attach_status("nvidia", None, true, true).1.as_deref(),
            Some("driver_not_loaded")
        );
        assert_eq!(
            attach_status("nvidia", Some("nvidia"), true, false),
            (true, None)
        );
        assert_eq!(
            attach_status("amd", Some("amdgpu"), false, false)
                .1
                .as_deref(),
            Some("no_render_node")
        );
        assert_eq!(
            attach_status("intel", Some("i915"), false, true),
            (true, None)
        );
        assert_eq!(
            attach_status("other", None, true, true).1.as_deref(),
            Some("unsupported_vendor")
        );
    }

    #[test]
    fn disk_transport_from_name_and_path() {
        assert_eq!(disk_transport("nvme0n1", ""), "nvme");
        assert_eq!(
            disk_transport("sda", "/sys/devices/pci0000:00/0000:00:17.0/ata1/host0"),
            "sata"
        );
        assert_eq!(
            disk_transport("sdb", "/sys/devices/pci0000:00/usb1/1-1"),
            "usb"
        );
        assert_eq!(disk_transport("vda", ""), "virtio");
        assert_eq!(disk_transport("mmcblk0", ""), "mmc");
        assert_eq!(disk_transport("sdc", ""), "scsi");
    }

    // The real machine: must never panic, and must agree with itself between
    // a fresh gather and the cached one.
    #[test]
    fn live_inventory_is_stable_and_cached() {
        let first = hardware_info(true);
        let second = hardware_info(false);
        assert_eq!(first, second);
        assert!(!first.machine.machine_type.is_empty());
    }
}
