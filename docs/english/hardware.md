# 🖥️ Hardware inventory and sensors (daemon)

> 🌍 **Language:** English · [🇷🇺 Русская версия](../russian/hardware.md)

## 📌 Description

What the node is made of, and how hot it is. The daemon builds a hardware inventory — machine type and virtualization, CPU, motherboard and firmware, memory modules, disks, GPUs — and reads the temperature and fan sensors of every device that has one. It is the data behind the platform's node **Resources** tab and behind the GPU picker in an app's settings, and it is available on its own through `asc hardware`.

Everything is read from procfs and sysfs. No new dependencies, and **no serial numbers**: the inventory is made to be shown in a UI and pasted into a support request, not to fingerprint the machine.

## 🎯 Scenarios

- `asc hardware` — a readable summary: machine, CPU, board, memory, disks, GPUs and current temperatures. `asc hardware --json` prints the same as one JSON document for scripts.
- "Is this a real server or a VPS, and which hypervisor?" — `machine_type` and `virtualization`.
- "Which GPU can I give to the container?" — the GPU list shows each card's PCI address and whether it can be attached.
- "What is overheating?" — the sensors are grouped per device: CPU package and cores, GPU, NVMe/SATA drives, motherboard chip, DIMMs, network cards, fans.
- The platform reads the same data through `MonitorService.GetHardwareInfo` and the metrics stream.

## 🏗️ Technical design

### 🧱 Inventory

`src/daemon/monitor/hardware.rs`. API: `MonitorService.GetHardwareInfo` (gRPC), REST `GET /v1/hardware[?refresh=1]`, capability `hardware`. The answer is cached for ten minutes — hardware barely changes while the daemon runs, and gathering it spawns helper processes — unless `refresh` is set.

| Block | Source |
|---|---|
| Machine type and virtualization | `systemd-detect-virt`; without it: PID 1's `container=`, `/.dockerenv`, `/proc/vz` (OpenVZ), the `hypervisor` CPU flag, WSL's kernel release, and DMI vendor/product strings (QEMU/KVM, VMware, Hyper-V, VirtualBox, Xen, Amazon EC2, Google Cloud, DigitalOcean, Hetzner, OpenStack…) |
| System, board, BIOS | `/sys/class/dmi/id/*`, with the usual "To Be Filled By O.E.M." placeholders dropped |
| CPU | `/proc/cpuinfo`: vendor, model, sockets, physical cores (unique `physical id`/`core id` pairs), threads, maximum frequency (`cpufreq`); ARM boards report the licensee from `CPU implementer` |
| Memory | total from `/proc/meminfo`; modules from the SMBIOS table (type 17 *Memory Device*): slot, size, type (DDR4/DDR5/LPDDR5…), rated and configured speed, manufacturer, part number |
| Disks | `/sys/block/*` without loop/ram/zram/device-mapper/RAID/optical devices: model, size, kind (`nvme`, `ssd`, `hdd`, `virtual`), bus (`nvme`, `sata`, `scsi`, `usb`, `virtio`, `mmc`) |
| GPUs | PCI devices of class `0x03`: PCI address, vendor, model, VRAM, bound driver, DRM render/card nodes, NVIDIA UUID |

- `machine_type` is `bare_metal`, `virtual_machine`, `container` or `unknown`. A container is decided first: it shares the host's kernel, so the CPU flags and DMI it sees are the host's.
- A virtual machine often hides DMI and the SMBIOS memory table. Those fields stay absent (never faked), and the UI says the provider does not expose them.
- GPU model: NVIDIA from `nvidia-smi --query-gpu=pci.bus_id,uuid,name,memory.total` (only run when an NVIDIA card is bound to the proprietary driver); otherwise `product_name`, then the system's `pci.ids` database, then `vendor:device`.
- **Attachability** (`attachable` / `attach_hint`): an NVIDIA card needs the proprietary driver bound and the NVIDIA Container Toolkit installed (`driver_not_loaded`, `toolkit_missing`); AMD and Intel cards need a DRM render node (`no_render_node`); anything else is `unsupported_vendor`.

### 🌡️ Sensors

`src/daemon/monitor/sensors.rs`. A temperature is a metric, not an inventory item, so the readings ride in `SystemMetrics.temperatures` / `fans` — on `GetSystemMetrics`, `StreamSystemMetrics` and REST `GET /v1/metrics`, and in `asc hardware`. They are read at most once a second (capability `sensors`).

- Source: `/sys/class/hwmon/hwmon*` (`temp*_input`, `_label`, `_max`, `_crit`, `fan*_input`); when hwmon is empty (ARM boards), `/sys/class/thermal/thermal_zone*`.
- Each reading is attached to its device — `kind` and `device_id` — by the chip's driver and the device node it hangs off:

| `kind` | Drivers | `device_id` |
|---|---|---|
| `cpu` | `coretemp`, `k10temp`, `zenpower`, `cpu_thermal` | `cpu` |
| `gpu` | `amdgpu`, `radeon`, `nouveau`, `i915`, `xe` | the PCI address, the same id as in the GPU list |
| `disk` | `nvme`, `drivetemp` | the NVMe controller or block device name |
| `board` | `nct67xx`, `it87`, `w83*`, `acpitz`, `pch_*` | `board` |
| `memory` | `jc42`, `spd5118` | the DIMM's bus address |
| `network` | network cards with hwmon | the interface name |
| `other` | everything else | the chip name |

- NVIDIA cards have no hwmon node: their temperature stays in `GpuMetrics.temperature_c`.
- Channels that read zero or an out-of-range value (a header with nothing wired to it) are dropped. `max_c` / `crit_c` are the driver's thresholds, absent when it has none.
- A virtual machine normally has no sensors; an empty list is a valid answer.

### 🎮 GPU passthrough

Selecting GPUs for an app is described in [📦 package-manager](package-manager.md#-gpu-passthrough): the `$gpus` setting holds PCI addresses from this inventory, and the daemon translates them into a `DeviceRequest` (NVIDIA) or device mappings (AMD/Intel) when it creates the container.

### ⌨️ CLI

`asc hardware [--json]` reads everything in-process, like the metrics block of `asc status` — no running daemon needed. Output is translated (EN/RU); `--json` is language-neutral.
