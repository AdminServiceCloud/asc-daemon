//! The `wg-quick` `.conf` format: reading any file (the daemon's
//! own, or one brought in by an import) into the model, writing the model
//! back, and building a client's config.
//!
//! The daemon's own metadata rides in `# asc:` comments, which `wg` and
//! `wg-quick` ignore; a disabled peer is a `[Peer]` block commented out with
//! the `#asc:off ` prefix.

use anyhow::{Result, bail};

use super::model::{Hooks, Interface, MARKER, Peer, is_host_route, normalize_nets};
use crate::daemon::firewall::model::parse_net;

/// Tag on the hook lines the daemon writes for NAT; they are rebuilt from the
/// `masquerade` flag, never read back as the operator's own hooks.
const NAT_TAG: &str = "# asc:nat";
const OFF_PREFIX: &str = "#asc:off ";

#[derive(Debug)]
pub struct Parsed {
    pub interface: Interface,
    /// The file carries the daemon's marker.
    pub managed: bool,
    /// What was dropped or not understood.
    pub warnings: Vec<String>,
}

enum Section {
    None,
    Interface,
    Peer,
}

/// Splits a `# asc:key = value` comment.
fn asc_comment(line: &str) -> Option<(&str, &str)> {
    let rest = line.trim().strip_prefix('#')?.trim_start();
    let body = rest.strip_prefix("asc:")?;
    Some(match body.split_once('=') {
        Some((k, v)) => (k.trim(), v.trim()),
        None => (body.trim(), ""),
    })
}

fn list(value: &str) -> impl Iterator<Item = String> + '_ {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Reads a `.conf`. Fails on structure it cannot make sense of (no
/// `[Interface]`, no private key, a line that is not `key = value`); the
/// values themselves are checked by [`Interface::normalize`].
pub fn parse(name: &str, text: &str) -> Result<Parsed> {
    let managed = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .is_some_and(|l| l.starts_with(MARKER));
    let mut interface = Interface::new(name, "");
    let mut warnings: Vec<String> = Vec::new();
    let mut section = Section::None;
    let mut seen_interface = false;
    let mut peer: Option<Peer> = None;

    let finish =
        |peer: &mut Option<Peer>, interface: &mut Interface, warnings: &mut Vec<String>| {
            if let Some(done) = peer.take() {
                if done.public_key.is_empty() {
                    warnings.push("a [Peer] block without a PublicKey was skipped".to_string());
                } else {
                    interface.peers.push(done);
                }
            }
        };

    for (index, raw) in text.lines().enumerate() {
        let number = index + 1;
        let mut line = raw.trim();
        let mut off = false;
        if let Some(rest) = line.strip_prefix(OFF_PREFIX.trim_end()) {
            line = rest.trim_start();
            off = true;
        }
        if line.is_empty() {
            continue;
        }
        if line.starts_with('#') {
            if let Some((key, value)) = asc_comment(line) {
                match (&section, key) {
                    (Section::Interface, "endpoint") => interface.endpoint = value.to_string(),
                    (Section::Interface, "masquerade") => interface.masquerade = true,
                    (Section::Interface, "client-dns") => {
                        interface.client_dns = list(value).collect();
                    }
                    (Section::Peer, "name") => {
                        if let Some(p) = peer.as_mut() {
                            p.name = value.to_string();
                        }
                    }
                    _ => {}
                }
            }
            continue;
        }
        if line.contains(NAT_TAG) {
            continue;
        }
        // A trailing comment is not part of the value (`wg-quick` cuts it too).
        let content = line.split('#').next().unwrap_or_default().trim();
        if content.is_empty() {
            continue;
        }
        if content.starts_with('[') {
            let header = content.to_ascii_lowercase();
            match header.as_str() {
                "[interface]" => {
                    if seen_interface {
                        bail!("line {number}: a second [Interface] section");
                    }
                    finish(&mut peer, &mut interface, &mut warnings);
                    seen_interface = true;
                    section = Section::Interface;
                }
                "[peer]" => {
                    finish(&mut peer, &mut interface, &mut warnings);
                    let mut p = Peer::new("");
                    p.enabled = !off;
                    peer = Some(p);
                    section = Section::Peer;
                }
                _ => bail!("line {number}: unknown section {content}"),
            }
            continue;
        }
        let Some((key, value)) = content.split_once('=') else {
            bail!("line {number}: expected `key = value`, found `{content}`");
        };
        let (key, value) = (key.trim().to_ascii_lowercase(), value.trim());
        match section {
            Section::None => bail!("line {number}: `{content}` is outside any section"),
            Section::Interface => {
                match key.as_str() {
                    "privatekey" => interface.private_key = value.to_string(),
                    "address" => interface.addresses.extend(list(value)),
                    "listenport" => {
                        interface.listen_port = Some(value.parse().map_err(|_| {
                            anyhow::anyhow!("line {number}: '{value}' is not a port")
                        })?);
                    }
                    "dns" => interface.dns.extend(list(value)),
                    "mtu" => {
                        interface.mtu = Some(value.parse().map_err(|_| {
                            anyhow::anyhow!("line {number}: '{value}' is not an MTU")
                        })?);
                    }
                    "table" => interface.table = value.to_string(),
                    "fwmark" => interface.fwmark = value.to_string(),
                    "preup" => interface.hooks.pre_up.push(value.to_string()),
                    "postup" => interface.hooks.post_up.push(value.to_string()),
                    "predown" => interface.hooks.pre_down.push(value.to_string()),
                    "postdown" => interface.hooks.post_down.push(value.to_string()),
                    "saveconfig" => {
                        if value.eq_ignore_ascii_case("true") {
                            warnings.push(
                            "SaveConfig = true was dropped: it would let wg-quick overwrite the file".to_string(),
                        );
                        }
                    }
                    _ => warnings.push(format!("the unknown key '{key}' was ignored")),
                }
            }
            Section::Peer => {
                let Some(p) = peer.as_mut() else { continue };
                match key.as_str() {
                    "publickey" => p.public_key = value.to_string(),
                    "presharedkey" => p.preshared_key = value.to_string(),
                    "allowedips" => p.allowed_ips.extend(list(value)),
                    "endpoint" => p.endpoint = value.to_string(),
                    "persistentkeepalive" => {
                        p.persistent_keepalive = if value.eq_ignore_ascii_case("off") {
                            0
                        } else {
                            value.parse().map_err(|_| {
                                anyhow::anyhow!(
                                    "line {number}: '{value}' is not a keepalive interval"
                                )
                            })?
                        };
                    }
                    _ => warnings.push(format!("the unknown peer key '{key}' was ignored")),
                }
            }
        }
    }
    finish(&mut peer, &mut interface, &mut warnings);

    if !seen_interface {
        bail!("the file has no [Interface] section");
    }
    if interface.private_key.is_empty() {
        bail!("the [Interface] section has no PrivateKey");
    }
    Ok(Parsed {
        interface,
        managed,
        warnings,
    })
}

// ── Writing ─────────────────────────────────────────────────────────────────

/// The nft table the NAT hooks use, one per interface.
fn nat_table(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("asc_wg_{safe}")
}

/// The hook lines NAT adds: `(post_up, post_down)`.
fn nat_hooks(i: &Interface) -> (Vec<String>, Vec<String>) {
    let Some(subnet) = i.ipv4_subnet().filter(|_| i.masquerade) else {
        return (Vec::new(), Vec::new());
    };
    let table = nat_table(&i.name);
    (
        vec![
            "sysctl -q -w net.ipv4.ip_forward=1".to_string(),
            format!(
                "nft 'add table ip {table}; add chain ip {table} postrouting {{ type nat hook postrouting priority srcnat; }}; add rule ip {table} postrouting ip saddr {subnet} oifname != \"{}\" masquerade'",
                i.name
            ),
        ],
        vec![format!("nft delete table ip {table} || true")],
    )
}

/// Renders the daemon's file for `i`.
pub fn render(i: &Interface) -> String {
    let mut out = String::new();
    out.push_str(MARKER);
    out.push_str(". Edit with `asc wireguard` or the panel; the `# asc:` comments carry\n");
    out.push_str("# the daemon's own settings and are read back.\n\n[Interface]\n");
    if !i.endpoint.is_empty() {
        out.push_str(&format!("# asc:endpoint = {}\n", i.endpoint));
    }
    if i.masquerade {
        out.push_str("# asc:masquerade\n");
    }
    if !i.client_dns.is_empty() {
        out.push_str(&format!("# asc:client-dns = {}\n", i.client_dns.join(", ")));
    }
    out.push_str(&format!("PrivateKey = {}\n", i.private_key));
    out.push_str(&format!("Address = {}\n", i.addresses.join(", ")));
    if let Some(port) = i.listen_port {
        out.push_str(&format!("ListenPort = {port}\n"));
    }
    if !i.dns.is_empty() {
        out.push_str(&format!("DNS = {}\n", i.dns.join(", ")));
    }
    if let Some(mtu) = i.mtu {
        out.push_str(&format!("MTU = {mtu}\n"));
    }
    if !i.table.is_empty() {
        out.push_str(&format!("Table = {}\n", i.table));
    }
    if !i.fwmark.is_empty() {
        out.push_str(&format!("FwMark = {}\n", i.fwmark));
    }
    let Hooks {
        pre_up,
        post_up,
        pre_down,
        post_down,
    } = &i.hooks;
    let (nat_up, nat_down) = nat_hooks(i);
    for cmd in pre_up {
        out.push_str(&format!("PreUp = {cmd}\n"));
    }
    for cmd in post_up {
        out.push_str(&format!("PostUp = {cmd}\n"));
    }
    for cmd in &nat_up {
        out.push_str(&format!("PostUp = {cmd} {NAT_TAG}\n"));
    }
    for cmd in pre_down {
        out.push_str(&format!("PreDown = {cmd}\n"));
    }
    for cmd in post_down {
        out.push_str(&format!("PostDown = {cmd}\n"));
    }
    for cmd in &nat_down {
        out.push_str(&format!("PostDown = {cmd} {NAT_TAG}\n"));
    }
    for peer in &i.peers {
        out.push('\n');
        let mut block = String::from("[Peer]\n");
        if !peer.name.is_empty() {
            block.push_str(&format!("# asc:name = {}\n", peer.name));
        }
        block.push_str(&format!("PublicKey = {}\n", peer.public_key));
        if !peer.preshared_key.is_empty() {
            block.push_str(&format!("PresharedKey = {}\n", peer.preshared_key));
        }
        if !peer.allowed_ips.is_empty() {
            block.push_str(&format!("AllowedIPs = {}\n", peer.allowed_ips.join(", ")));
        }
        if !peer.endpoint.is_empty() {
            block.push_str(&format!("Endpoint = {}\n", peer.endpoint));
        }
        if peer.persistent_keepalive > 0 {
            block.push_str(&format!(
                "PersistentKeepalive = {}\n",
                peer.persistent_keepalive
            ));
        }
        if peer.enabled {
            out.push_str(&block);
        } else {
            for line in block.lines() {
                out.push_str(OFF_PREFIX);
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

/// The text of a file with the secrets hidden: the private key and the
/// pre-shared keys, whether the line is live or part of a disabled peer.
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let (prefix, body) = match line.strip_prefix(OFF_PREFIX.trim_end()) {
            Some(rest) => (OFF_PREFIX.trim_end(), rest),
            None => ("", line),
        };
        let trimmed = body.trim_start();
        let key = trimmed
            .split_once('=')
            .map(|(k, _)| k.trim().to_ascii_lowercase());
        if !trimmed.starts_with('#')
            && matches!(key.as_deref(), Some("privatekey" | "presharedkey"))
        {
            let name = trimmed
                .split_once('=')
                .map(|(k, _)| k.trim())
                .unwrap_or_default();
            let lead = &body[..body.len() - trimmed.len()];
            out.push_str(&format!("{prefix}{lead}{name} = (hidden)\n"));
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

// ── The client's config ─────────────────────────────────────────────────────

/// What the client sends through the tunnel — the `AllowedIPs` of its config.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Routes {
    /// Everything: `0.0.0.0/0, ::/0`.
    Full,
    /// Only the VPN's own network.
    #[default]
    Subnet,
    Custom(Vec<String>),
}

impl Routes {
    /// `full`, `subnet` (or empty), or a list of networks.
    pub fn parse(value: &str) -> Result<Self> {
        let value = value.trim();
        Ok(match value.to_ascii_lowercase().as_str() {
            "" | "subnet" => Self::Subnet,
            "full" => Self::Full,
            _ => {
                let nets = normalize_nets(&list(value).collect::<Vec<_>>())?;
                if nets.is_empty() {
                    bail!("no routes given: use full, subnet or a list of networks");
                }
                Self::Custom(nets)
            }
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct ClientOptions {
    pub routes: Routes,
    /// Replaces the interface's DNS in the client config when not empty.
    pub dns: Vec<String>,
    /// Replaces the interface's public endpoint when not empty.
    pub endpoint: String,
}

fn bracket(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// The config the peer's device imports. `private_key` is known only when
/// the daemon generated the pair a moment ago; otherwise a placeholder
/// stands in. `fallback_host` is the node's own address, used when neither
/// the options nor the interface name a public endpoint.
pub fn client_config(
    server: &Interface,
    server_public_key: &str,
    peer: &Peer,
    private_key: Option<&str>,
    options: &ClientOptions,
    fallback_host: &str,
) -> Result<String> {
    let addresses: Vec<&String> = peer
        .allowed_ips
        .iter()
        .filter(|ip| is_host_route(ip))
        .collect();
    if addresses.is_empty() {
        bail!(
            "the peer has no host address (a /32 or /128 in AllowedIPs), so a client config has no Address to carry"
        );
    }
    let routes = match &options.routes {
        Routes::Full => vec!["0.0.0.0/0".to_string(), "::/0".to_string()],
        Routes::Subnet => server.subnets(),
        Routes::Custom(list) => list.clone(),
    };
    let host = [
        options.endpoint.as_str(),
        server.endpoint.as_str(),
        fallback_host,
    ]
    .into_iter()
    .map(str::trim)
    .find(|h| !h.is_empty())
    .unwrap_or("<SERVER_ADDRESS>");
    let dns: Vec<&String> = if options.dns.is_empty() {
        server.client_dns.iter().collect()
    } else {
        options.dns.iter().collect()
    };

    let mut out = String::from("[Interface]\n");
    out.push_str(&format!(
        "PrivateKey = {}\n",
        private_key.unwrap_or("<PRIVATE_KEY>")
    ));
    out.push_str(&format!(
        "Address = {}\n",
        addresses
            .iter()
            .map(|a| a.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    if !dns.is_empty() {
        out.push_str(&format!(
            "DNS = {}\n",
            dns.iter()
                .map(|d| d.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(mtu) = server.mtu {
        out.push_str(&format!("MTU = {mtu}\n"));
    }
    out.push_str("\n[Peer]\n");
    out.push_str(&format!("PublicKey = {server_public_key}\n"));
    if !peer.preshared_key.is_empty() {
        out.push_str(&format!("PresharedKey = {}\n", peer.preshared_key));
    }
    out.push_str(&format!(
        "Endpoint = {}:{}\n",
        bracket(host),
        server.port_or_default()
    ));
    out.push_str(&format!("AllowedIPs = {}\n", routes.join(", ")));
    if peer.persistent_keepalive > 0 {
        out.push_str(&format!(
            "PersistentKeepalive = {}\n",
            peer.persistent_keepalive
        ));
    }
    // A route the client could not parse is the operator's typo; fail here
    // rather than hand out a config the device refuses.
    for route in &routes {
        parse_net(route)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: char) -> String {
        format!("{}=", c.to_string().repeat(43))
    }

    fn server() -> Interface {
        let mut i = Interface::new("wg0", &key('P'));
        i.addresses = vec!["10.8.0.1/24".into()];
        i.listen_port = Some(51820);
        i.client_dns = vec!["1.1.1.1".into()];
        i.endpoint = "vpn.example.com".into();
        let mut p = Peer::new(&key('A'));
        p.name = "phone".into();
        p.preshared_key = key('S');
        p.allowed_ips = vec!["10.8.0.2/32".into()];
        p.persistent_keepalive = 25;
        i.peers = vec![p];
        i
    }

    #[test]
    fn a_rendered_file_reads_back_to_the_same_interface() {
        let mut original = server();
        original.mtu = Some(1380);
        let mut off = Peer::new(&key('B'));
        off.name = "lost laptop".into();
        off.enabled = false;
        off.allowed_ips = vec!["10.8.0.3/32".into(), "192.168.50.0/24".into()];
        off.endpoint = "203.0.113.5:51820".into();
        original.peers.push(off);
        let text = render(&original);
        assert!(text.starts_with(MARKER));
        let parsed = parse("wg0", &text).unwrap();
        assert!(parsed.managed && parsed.warnings.is_empty());
        assert_eq!(parsed.interface, original);
    }

    #[test]
    fn a_disabled_peer_is_commented_out_so_wireguard_never_loads_it() {
        let mut i = server();
        i.peers[0].enabled = false;
        let text = render(&i);
        assert!(!text.lines().any(|l| l.starts_with("PublicKey")), "{text}");
        assert!(text.contains("#asc:off [Peer]") && text.contains("#asc:off PublicKey"));
    }

    #[test]
    fn nat_writes_tagged_hooks_that_are_rebuilt_not_read_back() {
        let mut i = server();
        i.masquerade = true;
        i.hooks.post_up = vec!["echo mine".into()];
        let text = render(&i);
        assert!(text.contains("PostUp = sysctl -q -w net.ipv4.ip_forward=1 # asc:nat"));
        assert!(text.contains("ip saddr 10.8.0.0/24 oifname != \"wg0\" masquerade"));
        assert!(text.contains("PostDown = nft delete table ip asc_wg_wg0 || true # asc:nat"));
        let parsed = parse("wg0", &text).unwrap().interface;
        assert!(parsed.masquerade);
        assert_eq!(parsed.hooks.post_up, ["echo mine"]);
        assert!(parsed.hooks.post_down.is_empty());
        // Switching NAT off removes the lines and leaves the operator's hook.
        let mut off = parsed;
        off.masquerade = false;
        let text = render(&off);
        assert!(!text.contains("asc:nat") && !text.contains("nft"));
        assert!(text.contains("PostUp = echo mine"));
    }

    #[test]
    fn a_file_from_elsewhere_is_understood_and_what_is_dropped_is_reported() {
        let text = format!(
            "# my vpn\n[Interface]\nPrivateKey={}\nAddress = 10.0.0.1/24, fd00::1/64\nListenPort = 4000\nDNS = 9.9.9.9, example.org\nSaveConfig = true\nPostUp = iptables -A FORWARD -i %i -j ACCEPT # nat\nFoo = bar\n\n[Peer]\nPublicKey = {}\nAllowedIPs = 10.0.0.2/32\nAllowedIPs = 192.168.1.0/24\nPersistentKeepalive = off\n",
            key('P'),
            key('A')
        );
        let parsed = parse("office", &text).unwrap();
        assert!(!parsed.managed);
        let i = &parsed.interface;
        assert_eq!(i.addresses, ["10.0.0.1/24", "fd00::1/64"]);
        assert_eq!(i.listen_port, Some(4000));
        assert_eq!(i.dns, ["9.9.9.9", "example.org"]);
        assert_eq!(i.hooks.post_up, ["iptables -A FORWARD -i %i -j ACCEPT"]);
        assert_eq!(i.peers[0].allowed_ips, ["10.0.0.2/32", "192.168.1.0/24"]);
        assert_eq!(i.peers[0].persistent_keepalive, 0);
        assert!(parsed.warnings.iter().any(|w| w.contains("SaveConfig")));
        assert!(parsed.warnings.iter().any(|w| w.contains("'foo'")));
    }

    #[test]
    fn a_client_file_with_one_endpoint_peer_imports() {
        let text = format!(
            "[Interface]\nPrivateKey = {}\nAddress = 10.66.66.2/32\nDNS = 10.66.66.1\n\n[Peer]\nPublicKey = {}\nEndpoint = vpn.example.net:51820\nAllowedIPs = 0.0.0.0/0, ::/0\nPersistentKeepalive = 25\n",
            key('C'),
            key('D')
        );
        let mut i = parse("client", &text).unwrap().interface;
        assert_eq!(i.listen_port, None);
        assert_eq!(i.peers[0].endpoint, "vpn.example.net:51820");
        i.normalize().unwrap();
        assert_eq!(i.peers[0].allowed_ips, ["0.0.0.0/0", "::/0"]);
    }

    #[test]
    fn broken_files_are_refused_with_the_line() {
        assert!(
            parse("x", "")
                .unwrap_err()
                .to_string()
                .contains("no [Interface]")
        );
        assert!(
            parse("x", "[Interface]\nAddress = 10.0.0.1/24\n")
                .unwrap_err()
                .to_string()
                .contains("PrivateKey")
        );
        let err = parse(
            "x",
            &format!("[Interface]\nPrivateKey = {}\nnonsense\n", key('P')),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("line 3"), "{err}");
        assert!(
            parse("x", "PrivateKey = a\n")
                .unwrap_err()
                .to_string()
                .contains("outside")
        );
        assert!(
            parse("x", "[Foo]\n")
                .unwrap_err()
                .to_string()
                .contains("unknown section")
        );
        let two = format!(
            "[Interface]\nPrivateKey = {k}\n[Interface]\nPrivateKey = {k}\n",
            k = key('P')
        );
        assert!(parse("x", &two).unwrap_err().to_string().contains("second"));
    }

    #[test]
    fn a_peer_without_a_key_is_skipped_with_a_warning() {
        let text = format!(
            "[Interface]\nPrivateKey = {}\nAddress = 10.0.0.1/24\n[Peer]\nAllowedIPs = 10.0.0.2/32\n",
            key('P')
        );
        let parsed = parse("x", &text).unwrap();
        assert!(parsed.interface.peers.is_empty());
        assert!(parsed.warnings[0].contains("PublicKey"));
    }

    #[test]
    fn client_config_carries_the_chosen_routes() {
        let s = server();
        let peer = &s.peers[0];
        let cfg = |routes: Routes| {
            client_config(
                &s,
                &key('V'),
                peer,
                Some(&key('K')),
                &ClientOptions {
                    routes,
                    ..Default::default()
                },
                "198.51.100.7",
            )
            .unwrap()
        };
        let subnet = cfg(Routes::Subnet);
        assert!(subnet.contains(&format!("PrivateKey = {}", key('K'))));
        assert!(subnet.contains("Address = 10.8.0.2/32"));
        assert!(subnet.contains("DNS = 1.1.1.1"));
        assert!(subnet.contains(&format!("PublicKey = {}", key('V'))));
        assert!(subnet.contains(&format!("PresharedKey = {}", key('S'))));
        assert!(subnet.contains("Endpoint = vpn.example.com:51820"));
        assert!(subnet.contains("AllowedIPs = 10.8.0.0/24\n"));
        assert!(subnet.contains("PersistentKeepalive = 25"));
        assert!(cfg(Routes::Full).contains("AllowedIPs = 0.0.0.0/0, ::/0\n"));
        assert!(
            cfg(Routes::Custom(vec![
                "10.0.0.0/8".into(),
                "192.168.0.0/16".into()
            ]))
            .contains("AllowedIPs = 10.0.0.0/8, 192.168.0.0/16\n")
        );
    }

    #[test]
    fn client_config_endpoint_falls_back_and_brackets_ipv6() {
        let mut s = server();
        s.endpoint.clear();
        let peer = s.peers[0].clone();
        let fallback = client_config(
            &s,
            &key('V'),
            &peer,
            None,
            &ClientOptions::default(),
            "198.51.100.7",
        )
        .unwrap();
        assert!(fallback.contains("Endpoint = 198.51.100.7:51820"));
        assert!(fallback.contains("PrivateKey = <PRIVATE_KEY>"));
        let v6 = client_config(
            &s,
            &key('V'),
            &peer,
            None,
            &ClientOptions {
                endpoint: "2001:db8::1".into(),
                ..Default::default()
            },
            "",
        )
        .unwrap();
        assert!(v6.contains("Endpoint = [2001:db8::1]:51820"));
        let none =
            client_config(&s, &key('V'), &peer, None, &ClientOptions::default(), "").unwrap();
        assert!(none.contains("<SERVER_ADDRESS>:51820"));
    }

    #[test]
    fn a_peer_with_only_networks_has_no_client_config() {
        let mut s = server();
        s.peers[0].allowed_ips = vec!["192.168.50.0/24".into()];
        let err = client_config(
            &s,
            &key('V'),
            &s.peers[0],
            None,
            &ClientOptions::default(),
            "",
        )
        .unwrap_err();
        assert!(err.to_string().contains("no host address"));
    }

    #[test]
    fn secrets_are_hidden_in_the_view_of_a_file() {
        let mut i = server();
        i.peers[0].enabled = false;
        let shown = redact(&render(&i));
        assert!(
            !shown.contains(&key('P')) && !shown.contains(&key('S')),
            "{shown}"
        );
        assert!(shown.contains("PrivateKey = (hidden)"));
        assert!(shown.contains("#asc:off PresharedKey = (hidden)"));
        // Public keys and the rest stay readable.
        assert!(shown.contains(&key('A')) && shown.contains("Address = 10.8.0.1/24"));
        // A foreign file with odd spacing and case is covered as well.
        let foreign = redact(
            "[Interface]\nprivatekey=SECRET\n  PRESHAREDKEY =  OTHER  \n# PrivateKey = in a comment\n",
        );
        assert!(!foreign.contains("SECRET") && !foreign.contains("OTHER"));
        assert!(foreign.contains("# PrivateKey = in a comment"));
    }

    #[test]
    fn routes_parse_the_presets_and_lists() {
        assert_eq!(Routes::parse("").unwrap(), Routes::Subnet);
        assert_eq!(Routes::parse("Full").unwrap(), Routes::Full);
        assert_eq!(
            Routes::parse("10.0.0.0/8, 192.168.1.1").unwrap(),
            Routes::Custom(vec!["10.0.0.0/8".into(), "192.168.1.1/32".into()])
        );
        assert!(Routes::parse("10.0.0.0/99").is_err());
        assert!(Routes::parse(",").is_err());
    }
}
