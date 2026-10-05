//! The WireGuard model (DMN-152): an interface with its peers, what makes
//! each part valid, and handing out addresses to new peers. The `.conf` file
//! is the source of truth; `conf.rs` reads and writes it.

use std::collections::BTreeSet;
use std::net::IpAddr;

use anyhow::{Result, bail};
use serde::Serialize;

use crate::daemon::firewall::model::{Net, parse_net};

/// First line of every file the daemon manages.
pub const MARKER: &str = "# Managed by ASC";
/// The kernel limit on an interface name.
const MAX_NAME: usize = 15;
pub const MAX_PEERS: usize = 1000;
const DEFAULT_PORT: u16 = 51820;

/// Commands `wg-quick` runs around bringing the interface up and down. They
/// run as root, so they are shown to the operator and imported only on
/// request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Hooks {
    pub pre_up: Vec<String>,
    pub post_up: Vec<String>,
    pub pre_down: Vec<String>,
    pub post_down: Vec<String>,
}

impl Hooks {
    pub fn is_empty(&self) -> bool {
        self.pre_up.is_empty()
            && self.post_up.is_empty()
            && self.pre_down.is_empty()
            && self.post_down.is_empty()
    }

    /// `PostUp: cmd` lines, for showing them.
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (key, list) in [
            ("PreUp", &self.pre_up),
            ("PostUp", &self.post_up),
            ("PreDown", &self.pre_down),
            ("PostDown", &self.post_down),
        ] {
            out.extend(list.iter().map(|cmd| format!("{key}: {cmd}")));
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Peer {
    pub public_key: String,
    pub name: String,
    pub enabled: bool,
    /// Never leaves the daemon except inside a client config.
    #[serde(skip)]
    pub preshared_key: String,
    pub allowed_ips: Vec<String>,
    /// `host:port` the peer is reached at; empty for roaming clients.
    pub endpoint: String,
    /// Seconds; 0 is off.
    pub persistent_keepalive: u16,
}

impl Peer {
    pub fn new(public_key: &str) -> Self {
        Self {
            public_key: public_key.to_string(),
            name: String::new(),
            enabled: true,
            preshared_key: String::new(),
            allowed_ips: Vec::new(),
            endpoint: String::new(),
            persistent_keepalive: 0,
        }
    }

    /// Checks the peer and puts its fields in canonical form.
    pub fn normalize(&mut self) -> Result<()> {
        if !valid_key(&self.public_key) {
            bail!("the peer's public key is not a WireGuard key (44 characters of base64)");
        }
        if !self.preshared_key.is_empty() && !valid_key(&self.preshared_key) {
            bail!("the pre-shared key is not a WireGuard key (44 characters of base64)");
        }
        self.name = self.name.trim().to_string();
        if self.name.chars().count() > 64 || self.name.chars().any(char::is_control) {
            bail!("a peer name is limited to 64 characters without control characters");
        }
        self.allowed_ips = normalize_nets(&self.allowed_ips)?;
        self.endpoint = self.endpoint.trim().to_string();
        if !self.endpoint.is_empty() {
            self.endpoint = normalize_endpoint(&self.endpoint)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Interface {
    pub name: String,
    #[serde(skip)]
    pub private_key: String,
    pub listen_port: Option<u16>,
    pub addresses: Vec<String>,
    /// What `wg-quick` makes this host use for DNS while the tunnel is up (the
    /// `DNS =` line). For a tunnel that carries this host's own traffic, such as
    /// an imported client file; a server leaves it empty.
    pub dns: Vec<String>,
    /// What clients of this server are told to use (the `DNS =` of their config).
    pub client_dns: Vec<String>,
    pub mtu: Option<u16>,
    /// `wg-quick`'s routing table: empty (the default), `off`, `auto` or a name or number.
    pub table: String,
    pub fwmark: String,
    /// The public host clients connect to; used in client configs.
    pub endpoint: String,
    /// Masquerade the VPN subnet to the internet (hooks the daemon writes).
    pub masquerade: bool,
    pub hooks: Hooks,
    pub peers: Vec<Peer>,
}

impl Interface {
    pub fn new(name: &str, private_key: &str) -> Self {
        Self {
            name: name.to_string(),
            private_key: private_key.to_string(),
            listen_port: None,
            addresses: Vec::new(),
            dns: Vec::new(),
            client_dns: Vec::new(),
            mtu: None,
            table: String::new(),
            fwmark: String::new(),
            endpoint: String::new(),
            masquerade: false,
            hooks: Hooks::default(),
            peers: Vec::new(),
        }
    }

    /// Checks the whole interface, its peers included, and canonicalises it.
    pub fn normalize(&mut self) -> Result<()> {
        if !valid_name(&self.name) {
            bail!(
                "'{}' is not a usable interface name (1-{MAX_NAME} characters: letters, digits, _ = + . -)",
                self.name
            );
        }
        if !valid_key(&self.private_key) {
            bail!("the interface's private key is not a WireGuard key (44 characters of base64)");
        }
        if self.listen_port == Some(0) {
            bail!("the listen port must be 1-65535");
        }
        self.addresses = normalize_nets(&self.addresses)?;
        if self.addresses.is_empty() {
            bail!("an interface needs at least one address (for example 10.8.0.1/24)");
        }
        self.dns = self
            .dns
            .iter()
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty())
            .collect();
        if let Some(bad) = self.dns.iter().find(|d| !valid_dns(d)) {
            bail!("'{bad}' is not a DNS server address or search domain");
        }
        self.client_dns = self
            .client_dns
            .iter()
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty())
            .collect();
        if let Some(bad) = self.client_dns.iter().find(|d| !valid_dns(d)) {
            bail!("'{bad}' is not a DNS server address or search domain");
        }
        if let Some(mtu) = self.mtu
            && !(576..=9000).contains(&mtu)
        {
            bail!("the MTU must be between 576 and 9000");
        }
        self.table = self.table.trim().to_string();
        if !self.table.is_empty()
            && !self
                .table
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            bail!(
                "'{}' is not a routing table (off, auto, a name or a number)",
                self.table
            );
        }
        self.fwmark = self.fwmark.trim().to_string();
        if !self.fwmark.is_empty() && !valid_fwmark(&self.fwmark) {
            bail!(
                "'{}' is not a firewall mark (off, a number or 0x…)",
                self.fwmark
            );
        }
        self.endpoint = self.endpoint.trim().to_string();
        if !self.endpoint.is_empty() && !valid_host(&self.endpoint) {
            bail!("the public endpoint must be a host name or IP address, without a port");
        }
        for hook in [
            &self.hooks.pre_up,
            &self.hooks.post_up,
            &self.hooks.pre_down,
            &self.hooks.post_down,
        ]
        .into_iter()
        .flatten()
        {
            if hook.trim().is_empty() || hook.contains(['\n', '\r']) {
                bail!("a PreUp/PostUp/PreDown/PostDown command must be one non-empty line");
            }
        }
        if self.masquerade && self.ipv4_subnet().is_none() {
            bail!("NAT needs an IPv4 address on the interface");
        }
        if self.peers.len() > MAX_PEERS {
            bail!("an interface is limited to {MAX_PEERS} peers");
        }
        let mut keys = BTreeSet::new();
        let mut hosts: Vec<(String, String)> = Vec::new();
        for peer in &mut self.peers {
            peer.normalize()?;
            if !keys.insert(peer.public_key.clone()) {
                bail!("the public key {} is used by two peers", peer.public_key);
            }
            for ip in &peer.allowed_ips {
                if is_host_route(ip) {
                    if let Some((_, other)) = hosts.iter().find(|(h, _)| h == ip) {
                        bail!(
                            "{ip} is routed to two peers ({} and {})",
                            label(other),
                            label(&peer.name)
                        );
                    }
                    hosts.push((ip.clone(), peer.name.clone()));
                }
            }
        }
        Ok(())
    }

    /// The network of the first IPv4 address, `10.8.0.0/24` for `10.8.0.1/24`.
    pub fn ipv4_subnet(&self) -> Option<String> {
        self.addresses.iter().find_map(|a| {
            let net = parse_net(a).ok()?;
            net.addr.is_ipv4().then(|| network_of(&net))
        })
    }

    /// Every network the interface's own addresses sit in.
    pub fn subnets(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for a in &self.addresses {
            if let Ok(net) = parse_net(a) {
                let subnet = network_of(&net);
                if !out.contains(&subnet) {
                    out.push(subnet);
                }
            }
        }
        out
    }

    pub fn peer_index(&self, id: &str) -> Result<usize> {
        let id = id.trim();
        if let Some(i) = self.peers.iter().position(|p| p.public_key == id) {
            return Ok(i);
        }
        let by_name: Vec<usize> = self
            .peers
            .iter()
            .enumerate()
            .filter(|(_, p)| !id.is_empty() && p.name == id)
            .map(|(i, _)| i)
            .collect();
        match by_name.as_slice() {
            [one] => Ok(*one),
            [] => Err(crate::daemon::exec::not_found(format!(
                "no peer '{id}' on {} (give its public key or its name)",
                self.name
            ))),
            _ => Err(crate::daemon::exec::invalid(format!(
                "{} peers are called '{id}'; use the public key",
                by_name.len()
            ))),
        }
    }

    /// The networks routed through the tunnel that the interface's own
    /// subnets do not already cover. `wg-quick` installs a route for each at
    /// `up`, and `wg syncconf` does not, so a change here needs a restart.
    pub fn extra_routes(&self) -> BTreeSet<String> {
        let subnets: Vec<Net> = self
            .subnets()
            .iter()
            .filter_map(|s| parse_net(s).ok())
            .collect();
        self.peers
            .iter()
            .filter(|p| p.enabled)
            .flat_map(|p| p.allowed_ips.iter())
            .filter(|ip| {
                parse_net(ip).is_ok_and(|net| !subnets.iter().any(|subnet| contains(subnet, &net)))
            })
            .cloned()
            .collect()
    }

    /// One free host address per address family of the interface, in the
    /// form peers carry them (`10.8.0.2/32`, `fd00::2/128`).
    pub fn free_addresses(&self) -> Result<Vec<String>> {
        let mut used: BTreeSet<IpAddr> = BTreeSet::new();
        for a in &self.addresses {
            if let Ok(net) = parse_net(a) {
                used.insert(net.addr);
            }
        }
        for peer in &self.peers {
            for ip in &peer.allowed_ips {
                if let Ok(net) = parse_net(ip)
                    && net.prefix.is_none_or(|p| p == host_bits(&net))
                {
                    used.insert(net.addr);
                }
            }
        }
        let mut out = Vec::new();
        let mut seen_v4 = false;
        let mut seen_v6 = false;
        for a in &self.addresses {
            let net = parse_net(a)?;
            let first_of_family = if net.is_v6() {
                !std::mem::replace(&mut seen_v6, true)
            } else {
                !std::mem::replace(&mut seen_v4, true)
            };
            if !first_of_family {
                continue;
            }
            out.push(allocate(&net, &used)?);
        }
        Ok(out)
    }

    pub fn port_or_default(&self) -> u16 {
        self.listen_port.unwrap_or(DEFAULT_PORT)
    }
}

fn label(name: &str) -> String {
    if name.is_empty() {
        "an unnamed peer".to_string()
    } else {
        format!("'{name}'")
    }
}

// ── Syntax checks ───────────────────────────────────────────────────────────

/// A Linux interface name, restricted to what is safe in a file name, a unit
/// name (`wg-quick@<name>`) and a shell command.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name.len() <= MAX_NAME
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '=' | '+' | '.' | '-'))
}

/// A WireGuard key: 32 bytes in base64 (44 characters, one `=`).
pub fn valid_key(key: &str) -> bool {
    key.len() == 44
        && key.ends_with('=')
        && key[..43]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/')
}

fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && (host.parse::<IpAddr>().is_ok()
            || host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'))
}

fn valid_dns(value: &str) -> bool {
    value.parse::<IpAddr>().is_ok()
        || (value.len() <= 253
            && value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'))
}

fn valid_fwmark(value: &str) -> bool {
    value == "off"
        || value.parse::<u32>().is_ok()
        || value
            .strip_prefix("0x")
            .is_some_and(|h| !h.is_empty() && u32::from_str_radix(h, 16).is_ok())
}

/// `host:port`, with an IPv6 host in brackets.
pub fn normalize_endpoint(value: &str) -> Result<String> {
    let value = value.trim();
    let Some((host, port)) = value.rsplit_once(':') else {
        bail!("'{value}' is not an endpoint: use host:port");
    };
    let port: u16 = port
        .parse()
        .ok()
        .filter(|p| *p > 0)
        .ok_or_else(|| anyhow::anyhow!("'{value}' has an invalid port"))?;
    let bare = host.strip_prefix('[').and_then(|h| h.strip_suffix(']'));
    match bare {
        Some(v6) if v6.parse::<std::net::Ipv6Addr>().is_ok() => Ok(format!("[{v6}]:{port}")),
        Some(_) => bail!("'{value}' is not an endpoint: bad IPv6 address"),
        None if host.parse::<std::net::Ipv6Addr>().is_ok() => {
            bail!("'{value}' is ambiguous: put an IPv6 address in brackets, [{host}]:{port}")
        }
        None if valid_host(host) => Ok(format!("{host}:{port}")),
        None => bail!("'{value}' is not an endpoint: bad host"),
    }
}

/// Parses a list of addresses and networks; a bare address becomes a host
/// route (`/32`, `/128`) and nothing is repeated.
pub fn normalize_nets(list: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for item in list {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let net = parse_net(item).map_err(|e| anyhow::anyhow!("{e}"))?;
        let text = match net.prefix {
            Some(_) => net.to_string(),
            None => format!("{}/{}", net.addr, host_bits(&net)),
        };
        if !out.contains(&text) {
            out.push(text);
        }
    }
    Ok(out)
}

fn host_bits(net: &Net) -> u8 {
    if net.is_v6() { 128 } else { 32 }
}

pub fn is_host_route(value: &str) -> bool {
    parse_net(value)
        .map(|net| net.prefix == Some(host_bits(&net)))
        .unwrap_or(false)
}

// ── Arithmetic on networks ──────────────────────────────────────────────────

fn to_bits(addr: IpAddr) -> u128 {
    match addr {
        IpAddr::V4(a) => u128::from(u32::from(a)),
        IpAddr::V6(a) => u128::from(a),
    }
}

fn from_bits(v6: bool, bits: u128) -> IpAddr {
    if v6 {
        IpAddr::V6(std::net::Ipv6Addr::from(bits))
    } else {
        IpAddr::V4(std::net::Ipv4Addr::from(bits as u32))
    }
}

fn mask(v6: bool, prefix: u8) -> u128 {
    let width: u32 = if v6 { 128 } else { 32 };
    let prefix = u32::from(prefix).min(width);
    if prefix == 0 {
        0
    } else {
        let all = if v6 { u128::MAX } else { u128::from(u32::MAX) };
        (all << (width - prefix)) & all
    }
}

/// `10.8.0.1/24` → `10.8.0.0/24`; a bare address stays a host network.
pub fn network_of(net: &Net) -> String {
    let prefix = net.prefix.unwrap_or_else(|| host_bits(net));
    let base = to_bits(net.addr) & mask(net.is_v6(), prefix);
    format!("{}/{prefix}", from_bits(net.is_v6(), base))
}

/// Whether `outer` contains all of `inner`.
fn contains(outer: &Net, inner: &Net) -> bool {
    if outer.is_v6() != inner.is_v6() {
        return false;
    }
    let outer_prefix = outer.prefix.unwrap_or_else(|| host_bits(outer));
    let inner_prefix = inner.prefix.unwrap_or_else(|| host_bits(inner));
    let m = mask(outer.is_v6(), outer_prefix);
    outer_prefix <= inner_prefix && (to_bits(outer.addr) & m) == (to_bits(inner.addr) & m)
}

/// The first host address of `net` not in `used`, as a host route.
fn allocate(net: &Net, used: &BTreeSet<IpAddr>) -> Result<String> {
    let v6 = net.is_v6();
    let width = host_bits(net);
    let prefix = net.prefix.unwrap_or(width);
    // /31, /32 and /127, /128 have no hosts to give out.
    if prefix >= width - 1 {
        bail!(
            "{net} leaves no room for peers; use a larger network such as {}/24",
            if v6 { "fd00::" } else { "10.8.0.0" }
        );
    }
    let base = to_bits(net.addr) & mask(v6, prefix);
    let shift = u32::from(width) - u32::from(prefix);
    let size = if shift >= 127 {
        u128::MAX
    } else {
        1u128 << shift
    };
    let last = if v6 { size - 1 } else { size - 2 };
    // An IPv6 /64 is too large to scan; a day's worth of peers is far fewer.
    for host in 1..=last.min(1 << 20) {
        let ip = from_bits(v6, base + host);
        if !used.contains(&ip) {
            return Ok(format!("{ip}/{width}"));
        }
    }
    bail!("{} has no free addresses left", network_of(net))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> String {
        format!("{}{}=", "A".repeat(42), char::from(b'A' + n % 26))
    }

    fn iface() -> Interface {
        let mut i = Interface::new("wg0", &key(0));
        i.addresses = vec!["10.8.0.1/24".into()];
        i.listen_port = Some(51820);
        i
    }

    #[test]
    fn names_follow_the_kernel_and_unit_name_rules() {
        assert!(valid_name("wg0") && valid_name("office-vpn") && valid_name("a.b_c"));
        assert!(!valid_name("") && !valid_name("-wg0") && !valid_name("way-too-long-name-x"));
        assert!(!valid_name("wg 0") && !valid_name("wg/0") && !valid_name("wg;0"));
    }

    #[test]
    fn keys_are_forty_four_characters_of_base64() {
        assert!(valid_key(&key(1)));
        assert!(
            !valid_key("short=")
                && !valid_key(&"A".repeat(44))
                && !valid_key(&format!("{}=", "!".repeat(43)))
        );
    }

    #[test]
    fn host_addresses_become_host_routes_and_duplicates_collapse() {
        let out = normalize_nets(&[
            "10.8.0.2".into(),
            " 10.8.0.2/32 ".into(),
            "fd00::2".into(),
            "192.168.1.0/24".into(),
        ])
        .unwrap();
        assert_eq!(out, ["10.8.0.2/32", "fd00::2/128", "192.168.1.0/24"]);
        assert!(normalize_nets(&["10.8.0.2/40".into()]).is_err());
        assert!(normalize_nets(&["nope".into()]).is_err());
    }

    #[test]
    fn endpoints_need_a_port_and_ipv6_needs_brackets() {
        assert_eq!(
            normalize_endpoint("vpn.example.com:51820").unwrap(),
            "vpn.example.com:51820"
        );
        assert_eq!(
            normalize_endpoint("[2001:db8::1]:51820").unwrap(),
            "[2001:db8::1]:51820"
        );
        assert!(normalize_endpoint("2001:db8::1:51820").is_err());
        assert!(normalize_endpoint("vpn.example.com").is_err());
        assert!(normalize_endpoint("vpn.example.com:0").is_err());
        assert!(normalize_endpoint("vpn example:1").is_err());
    }

    #[test]
    fn network_of_masks_the_host_bits() {
        let n = |s: &str| network_of(&parse_net(s).unwrap());
        assert_eq!(n("10.8.0.1/24"), "10.8.0.0/24");
        assert_eq!(n("10.8.0.77"), "10.8.0.77/32");
        assert_eq!(n("fd00:1::5/64"), "fd00:1::/64");
        assert_eq!(n("0.0.0.0/0"), "0.0.0.0/0");
    }

    #[test]
    fn free_addresses_skip_the_server_and_taken_hosts() {
        let mut i = iface();
        assert_eq!(i.free_addresses().unwrap(), ["10.8.0.2/32"]);
        let mut a = Peer::new(&key(1));
        a.allowed_ips = vec!["10.8.0.2/32".into()];
        let mut b = Peer::new(&key(2));
        b.allowed_ips = vec!["10.8.0.3/32".into(), "192.168.50.0/24".into()];
        i.peers = vec![a, b];
        assert_eq!(i.free_addresses().unwrap(), ["10.8.0.4/32"]);
    }

    #[test]
    fn free_addresses_cover_both_families_and_report_exhaustion() {
        let mut i = iface();
        i.addresses = vec!["10.8.0.1/24".into(), "fd00:8::1/64".into()];
        assert_eq!(
            i.free_addresses().unwrap(),
            ["10.8.0.2/32", "fd00:8::2/128"]
        );

        let mut tiny = iface();
        tiny.addresses = vec!["10.8.0.1/30".into()];
        let mut p = Peer::new(&key(1));
        p.allowed_ips = vec!["10.8.0.2/32".into()];
        tiny.peers = vec![p];
        // 10.8.0.1 is the server, .2 a peer, .3 the broadcast address.
        assert!(
            tiny.free_addresses()
                .unwrap_err()
                .to_string()
                .contains("no free addresses")
        );

        let mut host = iface();
        host.addresses = vec!["10.8.0.1/32".into()];
        assert!(
            host.free_addresses()
                .unwrap_err()
                .to_string()
                .contains("no room")
        );
    }

    #[test]
    fn routes_outside_the_vpn_subnet_are_the_ones_that_need_a_restart() {
        let mut i = iface();
        let mut a = Peer::new(&key(1));
        a.allowed_ips = vec!["10.8.0.2/32".into()];
        let mut b = Peer::new(&key(2));
        b.allowed_ips = vec!["10.8.0.3/32".into(), "192.168.50.0/24".into()];
        let mut c = Peer::new(&key(3));
        c.enabled = false;
        c.allowed_ips = vec!["172.16.0.0/12".into()];
        i.peers = vec![a, b, c];
        assert_eq!(
            i.extra_routes().into_iter().collect::<Vec<_>>(),
            ["192.168.50.0/24"]
        );
    }

    #[test]
    fn two_peers_cannot_claim_one_host_address() {
        let mut i = iface();
        let mut a = Peer::new(&key(1));
        a.name = "laptop".into();
        a.allowed_ips = vec!["10.8.0.2/32".into()];
        let mut b = Peer::new(&key(2));
        b.allowed_ips = vec!["10.8.0.2".into()];
        i.peers = vec![a, b];
        let err = i.normalize().unwrap_err().to_string();
        assert!(
            err.contains("10.8.0.2/32") && err.contains("'laptop'") && err.contains("unnamed"),
            "{err}"
        );
    }

    #[test]
    fn an_interface_needs_an_address_and_a_sane_shape() {
        let mut i = iface();
        i.addresses.clear();
        assert!(
            i.normalize()
                .unwrap_err()
                .to_string()
                .contains("at least one address")
        );
        let mut i = iface();
        i.mtu = Some(100);
        assert!(i.normalize().is_err());
        let mut i = iface();
        i.dns = vec!["1.1.1.1".into(), "bad dns".into()];
        assert!(i.normalize().is_err());
        let mut i = iface();
        i.client_dns = vec!["bad dns".into()];
        assert!(i.normalize().is_err());
        let mut i = iface();
        i.masquerade = true;
        i.addresses = vec!["fd00::1/64".into()];
        assert!(i.normalize().unwrap_err().to_string().contains("IPv4"));
        let mut i = iface();
        i.hooks.post_up = vec!["a\nb".into()];
        assert!(i.normalize().is_err());
        assert!(iface().normalize().is_ok());
    }

    #[test]
    fn a_peer_is_found_by_key_or_by_unique_name() {
        let mut i = iface();
        let mut a = Peer::new(&key(1));
        a.name = "phone".into();
        let mut b = Peer::new(&key(2));
        b.name = "phone".into();
        let mut c = Peer::new(&key(3));
        c.name = "laptop".into();
        i.peers = vec![a, b, c];
        assert_eq!(i.peer_index(&key(2)).unwrap(), 1);
        assert_eq!(i.peer_index("laptop").unwrap(), 2);
        assert!(
            i.peer_index("phone")
                .unwrap_err()
                .to_string()
                .contains("2 peers")
        );
        assert!(i.peer_index("nobody").is_err());
        assert!(i.peer_index("").is_err());
    }
}
