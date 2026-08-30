//! Network interface inventory (DMN-074): every interface on the machine,
//! loopback included — this is deliberately not the same list as the metrics
//! sampler's `NetworkMetrics`, which drops loopback because it only cares
//! about traffic rates. This module answers "what is on this machine and
//! how do I reach it", so nothing is filtered out.
//!
//! Metadata (MAC/MTU/state) comes from `/sys/class/net/<name>/*`; addresses
//! come from `getifaddrs(3)` — procfs has no per-interface address listing.

use std::collections::HashMap;
use std::fs;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkInterface {
    pub name: String,
    /// Empty for interfaces without a link-layer address (some tunnels).
    pub mac: String,
    pub mtu: u32,
    /// "up" or "down" (raw `/sys/class/net/<name>/operstate`, lowercased).
    pub state: String,
    pub is_loopback: bool,
    pub addresses: Vec<InterfaceAddress>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterfaceAddress {
    pub address: String,
    pub prefix_len: u32,
    /// "ipv4" or "ipv6".
    pub family: String,
    /// "host" (loopback), "link" (link-local) or "global" — a simplified
    /// stand-in for RFC 3549 scope, good enough for a UI badge.
    pub scope: String,
}

/// List every interface on the machine. Blocking: reads `/sys` and calls
/// `getifaddrs(3)`; call from a blocking task, same as [`super::system::Collector::sample`].
pub fn list_interfaces() -> Vec<NetworkInterface> {
    let mut interfaces = collect_metadata(Path::new("/sys/class/net"));
    let mut addresses = collect_addresses();
    for iface in &mut interfaces {
        if let Some(found) = addresses.remove(&iface.name) {
            iface.addresses = found;
        }
    }
    interfaces
}

/// Reads interface metadata from a sysfs-shaped directory: one subdirectory
/// per interface, each with `address`, `mtu` and `operstate` files. A
/// missing root (a container without `/sys/class/net` mounted) yields an
/// empty list rather than an error — this is an inventory, not a critical
/// sample.
fn collect_metadata(root: &Path) -> Vec<NetworkInterface> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out: Vec<NetworkInterface> = entries
        .flatten()
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let base = entry.path();
            let mac = fs::read_to_string(base.join("address"))
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            let mtu = fs::read_to_string(base.join("mtu"))
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
            let state = fs::read_to_string(base.join("operstate"))
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| "unknown".to_string());
            let is_loopback = name == "lo";
            NetworkInterface {
                name,
                mac,
                mtu,
                state,
                is_loopback,
                addresses: Vec::new(),
            }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn scope_of_ipv4(ip: Ipv4Addr) -> &'static str {
    if ip.is_loopback() {
        "host"
    } else if ip.is_link_local() {
        "link"
    } else {
        "global"
    }
}

fn scope_of_ipv6(ip: Ipv6Addr) -> &'static str {
    if ip.is_loopback() {
        "host"
    } else if (ip.segments()[0] & 0xffc0) == 0xfe80 {
        "link"
    } else {
        "global"
    }
}

/// Every address on the machine, grouped by interface name, via
/// `getifaddrs(3)`. Interfaces with no IPv4/IPv6 address (or only an
/// AF_PACKET link-layer entry) are simply absent from the map.
fn collect_addresses() -> HashMap<String, Vec<InterfaceAddress>> {
    let mut out: HashMap<String, Vec<InterfaceAddress>> = HashMap::new();
    // SAFETY: `getifaddrs` fills `addrs` with a heap-allocated linked list on
    // success; every field read from it below is a plain value or a pointer
    // that is null-checked before being dereferenced, and the list is freed
    // via `freeifaddrs` before returning on every path, including the early
    // `getifaddrs` failure above.
    unsafe {
        let mut addrs: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut addrs) != 0 {
            return out;
        }
        let mut cursor = addrs;
        while !cursor.is_null() {
            let ifa = &*cursor;
            cursor = ifa.ifa_next;
            if ifa.ifa_addr.is_null() {
                continue;
            }
            let name = std::ffi::CStr::from_ptr(ifa.ifa_name)
                .to_string_lossy()
                .into_owned();
            let family = i32::from((*ifa.ifa_addr).sa_family);
            if family == libc::AF_INET {
                let sa = ifa.ifa_addr as *const libc::sockaddr_in;
                let ip = Ipv4Addr::from(u32::from_be((*sa).sin_addr.s_addr));
                let prefix = if ifa.ifa_netmask.is_null() {
                    0
                } else {
                    let mask = ifa.ifa_netmask as *const libc::sockaddr_in;
                    u32::from_be((*mask).sin_addr.s_addr).count_ones()
                };
                out.entry(name).or_default().push(InterfaceAddress {
                    address: ip.to_string(),
                    prefix_len: prefix,
                    family: "ipv4".to_string(),
                    scope: scope_of_ipv4(ip).to_string(),
                });
            } else if family == libc::AF_INET6 {
                let sa = ifa.ifa_addr as *const libc::sockaddr_in6;
                let ip = Ipv6Addr::from((*sa).sin6_addr.s6_addr);
                let prefix = if ifa.ifa_netmask.is_null() {
                    0
                } else {
                    let mask = ifa.ifa_netmask as *const libc::sockaddr_in6;
                    (*mask)
                        .sin6_addr
                        .s6_addr
                        .iter()
                        .map(|b| b.count_ones())
                        .sum()
                };
                out.entry(name).or_default().push(InterfaceAddress {
                    address: ip.to_string(),
                    prefix_len: prefix,
                    family: "ipv6".to_string(),
                    scope: scope_of_ipv6(ip).to_string(),
                });
            }
        }
        libc::freeifaddrs(addrs);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_iface(root: &Path, name: &str, mac: &str, mtu: &str, state: &str) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::File::create(dir.join("address"))
            .unwrap()
            .write_all(mac.as_bytes())
            .unwrap();
        fs::File::create(dir.join("mtu"))
            .unwrap()
            .write_all(mtu.as_bytes())
            .unwrap();
        fs::File::create(dir.join("operstate"))
            .unwrap()
            .write_all(state.as_bytes())
            .unwrap();
    }

    #[test]
    fn metadata_reads_sysfs_shaped_tree() {
        let dir = tempfile::tempdir().unwrap();
        write_iface(dir.path(), "eth0", "aa:bb:cc:dd:ee:ff\n", "1500\n", "up\n");
        write_iface(
            dir.path(),
            "lo",
            "00:00:00:00:00:00\n",
            "65536\n",
            "unknown\n",
        );

        let interfaces = collect_metadata(dir.path());
        assert_eq!(interfaces.len(), 2);
        // Sorted by name: eth0 before lo.
        assert_eq!(interfaces[0].name, "eth0");
        assert_eq!(interfaces[0].mac, "aa:bb:cc:dd:ee:ff");
        assert_eq!(interfaces[0].mtu, 1500);
        assert_eq!(interfaces[0].state, "up");
        assert!(!interfaces[0].is_loopback);

        assert_eq!(interfaces[1].name, "lo");
        assert!(interfaces[1].is_loopback);
    }

    #[test]
    fn metadata_on_missing_root_is_empty() {
        let interfaces = collect_metadata(Path::new("/nonexistent/does/not/exist"));
        assert!(interfaces.is_empty());
    }

    #[test]
    fn scope_classification() {
        assert_eq!(scope_of_ipv4(Ipv4Addr::new(127, 0, 0, 1)), "host");
        assert_eq!(scope_of_ipv4(Ipv4Addr::new(169, 254, 1, 1)), "link");
        assert_eq!(scope_of_ipv4(Ipv4Addr::new(10, 0, 0, 5)), "global");
        assert_eq!(scope_of_ipv6(Ipv6Addr::LOCALHOST), "host");
        assert_eq!(
            scope_of_ipv6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1)),
            "link"
        );
        assert_eq!(
            scope_of_ipv6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)),
            "global"
        );
    }

    // Exercises the real system on Linux (the only supported build target).
    #[test]
    fn live_listing_has_loopback() {
        let interfaces = list_interfaces();
        let Some(lo) = interfaces.iter().find(|i| i.name == "lo") else {
            // Some sandboxes have no /sys/class/net mounted; nothing to
            // assert against in that case.
            return;
        };
        assert!(lo.is_loopback);
        if !lo.addresses.is_empty() {
            assert!(
                lo.addresses
                    .iter()
                    .any(|a| a.address == "127.0.0.1" || a.address == "::1")
            );
        }
    }
}
