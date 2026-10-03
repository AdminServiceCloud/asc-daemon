//! GPU passthrough for Docker apps (DMN-143).
//!
//! The user picks cards by PCI address in the app's `$gpus` setting. This
//! module turns those addresses into what the Engine needs, using the live
//! hardware inventory ([`crate::daemon::monitor::hardware`]):
//!
//!   - **NVIDIA** — a `DeviceRequest` for the card's UUID. The NVIDIA
//!     Container Toolkit's hook mounts the driver libraries and device nodes;
//!     the daemon only has to name the card.
//!   - **AMD / Intel** — plain device nodes: the card's DRM render node and
//!     card node, plus `/dev/kfd` (the ROCm compute interface) when it
//!     exists. These are bind-mounted into the container as devices.
//!
//! Addresses rather than indices, because an index shifts when a card is
//! added or removed, and the same address means the same slot after a reboot.
//! A card that is gone or cannot be attached is an error naming the address:
//! silently starting the app without its GPU would look like a slow app, not
//! a missing card.

use anyhow::{Result, bail};

use crate::daemon::monitor::hardware::{self, HardwareInfo};

/// What a container must be given to use the selected cards.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GpuGrant {
    /// NVIDIA card UUIDs, sorted: `DeviceRequest.device_ids`.
    pub nvidia_ids: Vec<String>,
    /// Device nodes to map one-to-one into the container, sorted.
    pub devices: Vec<String>,
}

impl GpuGrant {
    pub fn is_empty(&self) -> bool {
        self.nvidia_ids.is_empty() && self.devices.is_empty()
    }
}

/// Resolve PCI addresses against an inventory. `has_kfd` says whether
/// `/dev/kfd` exists on this host (passed in so the function stays pure).
pub fn resolve(addresses: &[String], inventory: &HardwareInfo, has_kfd: bool) -> Result<GpuGrant> {
    let mut grant = GpuGrant::default();
    for address in addresses {
        let Some(gpu) = inventory.gpus.iter().find(|gpu| &gpu.id == address) else {
            bail!("GPU {address} is not present on this host");
        };
        if !gpu.attachable {
            bail!(
                "GPU {address} ({}) cannot be attached to a container: {}",
                gpu.model,
                hint_text(gpu.attach_hint.as_deref())
            );
        }
        match gpu.vendor.as_str() {
            "nvidia" => {
                let Some(uuid) = &gpu.uuid else {
                    bail!("GPU {address} ({}) has no UUID to address it by", gpu.model);
                };
                grant.nvidia_ids.push(uuid.clone());
            }
            _ => {
                grant.devices.extend(gpu.render_node.clone());
                grant.devices.extend(gpu.card_node.clone());
                if gpu.vendor == "amd" && has_kfd {
                    grant.devices.push("/dev/kfd".to_string());
                }
            }
        }
    }
    grant.nvidia_ids.sort();
    grant.nvidia_ids.dedup();
    grant.devices.sort();
    grant.devices.dedup();
    Ok(grant)
}

/// Resolve against the live host. The cached inventory is tried first; a card
/// it does not know (hot-plugged since it was taken) earns one fresh read.
pub fn grant_for(addresses: &[String]) -> Result<GpuGrant> {
    if addresses.is_empty() {
        return Ok(GpuGrant::default());
    }
    let has_kfd = std::path::Path::new("/dev/kfd").exists();
    match resolve(addresses, &hardware::hardware_info(false), has_kfd) {
        Ok(grant) => Ok(grant),
        Err(_) => resolve(addresses, &hardware::hardware_info(true), has_kfd),
    }
}

fn hint_text(code: Option<&str>) -> &'static str {
    match code {
        Some("driver_not_loaded") => "the vendor driver is not loaded",
        Some("toolkit_missing") => "the NVIDIA Container Toolkit is not installed",
        Some("no_render_node") => "the card has no DRM render node",
        Some("unsupported_vendor") => "this GPU vendor is not supported for passthrough",
        _ => "not available",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::monitor::hardware::GpuInfo;

    fn nvidia(id: &str, uuid: &str) -> GpuInfo {
        GpuInfo {
            id: id.into(),
            vendor: "nvidia".into(),
            model: "NVIDIA GeForce RTX 3090".into(),
            uuid: Some(uuid.into()),
            attachable: true,
            ..GpuInfo::default()
        }
    }

    fn amd(id: &str, render: &str, card: &str) -> GpuInfo {
        GpuInfo {
            id: id.into(),
            vendor: "amd".into(),
            model: "Radeon RX 6700 XT".into(),
            render_node: Some(render.into()),
            card_node: Some(card.into()),
            attachable: true,
            ..GpuInfo::default()
        }
    }

    fn inventory(gpus: Vec<GpuInfo>) -> HardwareInfo {
        HardwareInfo {
            gpus,
            ..HardwareInfo::default()
        }
    }

    fn addresses(list: &[&str]) -> Vec<String> {
        list.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn nvidia_cards_are_addressed_by_uuid() {
        let inv = inventory(vec![
            nvidia("0000:01:00.0", "GPU-b"),
            nvidia("0000:02:00.0", "GPU-a"),
        ]);
        let grant = resolve(&addresses(&["0000:01:00.0", "0000:02:00.0"]), &inv, false).unwrap();
        assert_eq!(grant.nvidia_ids, vec!["GPU-a", "GPU-b"]);
        assert!(grant.devices.is_empty());
    }

    #[test]
    fn amd_cards_map_their_device_nodes_and_kfd_when_present() {
        let inv = inventory(vec![amd(
            "0000:03:00.0",
            "/dev/dri/renderD128",
            "/dev/dri/card1",
        )]);
        let with_kfd = resolve(&addresses(&["0000:03:00.0"]), &inv, true).unwrap();
        assert_eq!(
            with_kfd.devices,
            vec!["/dev/dri/card1", "/dev/dri/renderD128", "/dev/kfd"]
        );
        let without = resolve(&addresses(&["0000:03:00.0"]), &inv, false).unwrap();
        assert_eq!(
            without.devices,
            vec!["/dev/dri/card1", "/dev/dri/renderD128"]
        );
        assert!(without.nvidia_ids.is_empty());
    }

    #[test]
    fn a_mixed_selection_and_duplicates_are_merged() {
        let inv = inventory(vec![
            nvidia("0000:01:00.0", "GPU-a"),
            amd("0000:03:00.0", "/dev/dri/renderD128", "/dev/dri/card1"),
        ]);
        let grant = resolve(
            &addresses(&["0000:03:00.0", "0000:01:00.0", "0000:01:00.0"]),
            &inv,
            false,
        )
        .unwrap();
        assert_eq!(grant.nvidia_ids, vec!["GPU-a"]);
        assert_eq!(grant.devices.len(), 2);
    }

    #[test]
    fn a_missing_card_is_named() {
        let inv = inventory(vec![nvidia("0000:01:00.0", "GPU-a")]);
        let err = resolve(&addresses(&["0000:09:00.0"]), &inv, false).unwrap_err();
        assert!(err.to_string().contains("0000:09:00.0"), "{err}");
    }

    #[test]
    fn a_card_that_cannot_be_attached_says_why() {
        let mut card = nvidia("0000:01:00.0", "GPU-a");
        card.attachable = false;
        card.attach_hint = Some("toolkit_missing".into());
        let err = resolve(&addresses(&["0000:01:00.0"]), &inventory(vec![card]), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("Container Toolkit"), "{err}");
        assert!(err.contains("RTX 3090"), "{err}");
    }

    #[test]
    fn no_selection_is_an_empty_grant() {
        let grant = resolve(&[], &inventory(vec![]), true).unwrap();
        assert!(grant.is_empty());
        assert!(grant_for(&[]).unwrap().is_empty());
    }
}
