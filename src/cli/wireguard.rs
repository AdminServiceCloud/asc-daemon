//! `asc wireguard` (DMN-152): the node's WireGuard through the running daemon.

use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;

use clap::Subcommand;
use serde_json::{Value, json};

use asc_daemon::daemon::client::Daemon;
use asc_daemon::daemon::config::Config;
use asc_daemon::daemon::i18n::{Msg, t, tf, tf2};

use super::daemon_backend;

#[derive(Subcommand)]
pub enum WireguardAction {
    /// Show WireGuard: the tunnels with their peers, handshakes and traffic
    Status,
    /// Install wireguard-tools
    Install,
    /// Remove wireguard-tools and stop the tunnels the daemon manages
    Uninstall {
        /// Also delete the managed configuration files (they hold the private keys)
        #[arg(long)]
        purge: bool,
    },
    /// Create a tunnel with a fresh key and bring it up
    Add(AddArgs),
    /// Change a tunnel's address, port, DNS, MTU, public endpoint or NAT
    Set(SetArgs),
    /// Delete a tunnel
    Remove { name: String },
    /// Bring a tunnel up and enable it at boot
    Up { name: String },
    /// Take a tunnel down and disable it at boot
    Down { name: String },
    /// Show a tunnel's configuration file, keys hidden
    Show { name: String },
    /// Import a ready .conf, a server's or a client's
    Import(ImportArgs),
    /// Manage the peers of a tunnel
    Peer {
        #[command(subcommand)]
        action: PeerAction,
    },
}

#[derive(Subcommand)]
pub enum PeerAction {
    /// Add a peer: next free address, generated keys, the client config
    Add(PeerAddArgs),
    /// Change a peer: name, AllowedIPs, endpoint, keepalive, on or off
    Set(PeerSetArgs),
    /// Remove a peer
    Remove {
        /// The tunnel
        interface: String,
        /// The peer's public key or name
        peer: String,
    },
    /// Print an existing peer's client config (the private key is a placeholder)
    Config(PeerConfigArgs),
}

#[derive(clap::Args)]
pub struct AddArgs {
    /// Interface name, for example wg0
    name: String,
    /// The tunnel's address and network, for example 10.8.0.1/24. Repeatable
    #[arg(long, value_name = "CIDR", required = true)]
    address: Vec<String>,
    /// UDP port to listen on; 0 makes a client-style tunnel that does not listen
    #[arg(long, default_value_t = 51820)]
    port: u16,
    /// DNS server handed to the clients. Repeatable
    #[arg(long, value_name = "ADDRESS")]
    dns: Vec<String>,
    #[arg(long)]
    mtu: Option<u16>,
    /// The public host name or address clients connect to
    #[arg(long, value_name = "HOST")]
    endpoint: Option<String>,
    /// Masquerade the VPN network to the internet (IPv4)
    #[arg(long)]
    nat: bool,
}

#[derive(clap::Args)]
pub struct SetArgs {
    name: String,
    /// Replace the addresses. Repeatable
    #[arg(long, value_name = "CIDR")]
    address: Vec<String>,
    /// UDP port; 0 stops listening
    #[arg(long)]
    port: Option<u16>,
    /// Replace the DNS servers handed to the clients. Repeatable
    #[arg(long, value_name = "ADDRESS")]
    dns: Vec<String>,
    /// MTU; 0 restores the default
    #[arg(long)]
    mtu: Option<u16>,
    /// The public host clients connect to; an empty value clears it
    #[arg(long, value_name = "HOST")]
    endpoint: Option<String>,
    /// Masquerade the VPN network to the internet
    #[arg(long, value_name = "BOOL")]
    nat: Option<bool>,
}

#[derive(clap::Args)]
pub struct ImportArgs {
    /// The .conf file, or - for standard input
    file: String,
    /// The tunnel name; defaults to the file name without .conf
    #[arg(long)]
    name: Option<String>,
    /// Replace a tunnel of that name (the old file is kept as .conf.asc-bak)
    #[arg(long)]
    overwrite: bool,
    /// Allow PreUp/PostUp/PreDown/PostDown commands from the file (they run as root)
    #[arg(long)]
    accept_hooks: bool,
    /// Bring the tunnel up and enable it at boot
    #[arg(long)]
    start: bool,
}

#[derive(clap::Args)]
pub struct PeerAddArgs {
    /// The tunnel
    interface: String,
    /// A name for the peer, for example phone
    name: String,
    /// Use this public key instead of generating a pair (the client keeps its private key)
    #[arg(long, value_name = "KEY")]
    public_key: Option<String>,
    /// The peer's addresses and the networks behind it; default: the next free address
    #[arg(long, value_name = "CIDR")]
    allowed_ips: Vec<String>,
    /// Do not add a pre-shared key
    #[arg(long)]
    no_psk: bool,
    /// Keepalive in seconds; 0 is off
    #[arg(long, default_value_t = 25)]
    keepalive: u16,
    /// Where the peer is reached (host:port); leave empty for a roaming client
    #[arg(long, value_name = "HOST:PORT")]
    endpoint: Option<String>,
    #[command(flatten)]
    client: ClientArgs,
}

#[derive(clap::Args)]
pub struct PeerSetArgs {
    /// The tunnel
    interface: String,
    /// The peer's public key or name
    peer: String,
    /// Rename the peer
    #[arg(long)]
    name: Option<String>,
    /// Replace the peer's AllowedIPs (addresses and networks routed to it)
    #[arg(long, value_name = "CIDR")]
    allowed_ips: Vec<String>,
    #[arg(long, value_name = "HOST:PORT")]
    endpoint: Option<String>,
    #[arg(long)]
    keepalive: Option<u16>,
    /// Load the peer again
    #[arg(long, conflicts_with = "disable")]
    enable: bool,
    /// Keep the peer in the file but cut it off
    #[arg(long)]
    disable: bool,
}

#[derive(clap::Args)]
pub struct PeerConfigArgs {
    /// The tunnel
    interface: String,
    /// The peer's public key or name
    peer: String,
    #[command(flatten)]
    client: ClientArgs,
}

#[derive(clap::Args)]
pub struct ClientArgs {
    /// What the client sends through the tunnel (its AllowedIPs): full, subnet or
    /// a list such as 10.8.0.0/24,192.168.10.0/24
    #[arg(long, default_value = "subnet", value_name = "full|subnet|LIST")]
    routes: String,
    /// DNS for the client instead of the tunnel's
    #[arg(long, value_name = "ADDRESS")]
    client_dns: Vec<String>,
    /// Public host for the client instead of the tunnel's
    #[arg(long, value_name = "HOST")]
    client_endpoint: Option<String>,
    /// Write the config to this file (mode 0600) instead of printing it
    #[arg(short, long, value_name = "FILE")]
    output: Option<String>,
}

impl ClientArgs {
    fn json(&self) -> Value {
        json!({
            "routes": self.routes,
            "dns": self.client_dns,
            "endpoint": self.client_endpoint.clone().unwrap_or_default(),
        })
    }
}

pub fn run(action: WireguardAction, config: &Config) -> anyhow::Result<()> {
    let Some(d) = daemon_backend(config)? else {
        anyhow::bail!("{}", t(Msg::WgDaemonRequired));
    };
    match action {
        WireguardAction::Status => status(&d),
        WireguardAction::Install => {
            eprintln!("{}", t(Msg::WgInstalling));
            let json = d.web("POST", "/v1/wireguard/install", None)?;
            print_log(&json);
            println!(
                "{}",
                tf(Msg::WgInstalled, text(&json["wireguard"], "version"))
            );
            Ok(())
        }
        WireguardAction::Uninstall { purge } => {
            let json = d.web("DELETE", &format!("/v1/wireguard?purge={purge}"), None)?;
            print_log(&json);
            println!("{}", t(Msg::WgUninstalled));
            Ok(())
        }
        WireguardAction::Add(args) => {
            overview(&d)?;
            let mut body = json!({
                "listen_port": args.port,
                "addresses": args.address,
                "client_dns": args.dns,
                "masquerade": args.nat,
                "endpoint": args.endpoint.unwrap_or_default(),
            });
            if let Some(mtu) = args.mtu {
                body["mtu"] = json!(mtu);
            }
            // 0 is "do not listen": the REST body takes null for it.
            if args.port == 0 {
                body["listen_port"] = Value::Null;
            }
            d.web(
                "PUT",
                &format!("/v1/wireguard/interfaces/{}", args.name),
                Some(body),
            )?;
            println!("{}", tf(Msg::WgSaved, &args.name));
            Ok(())
        }
        WireguardAction::Set(args) => set(&d, args),
        WireguardAction::Remove { name } => {
            overview(&d)?;
            d.web("DELETE", &format!("/v1/wireguard/interfaces/{name}"), None)?;
            println!("{}", tf(Msg::WgRemoved, &name));
            Ok(())
        }
        WireguardAction::Up { name } => switch(&d, &name, true),
        WireguardAction::Down { name } => switch(&d, &name, false),
        WireguardAction::Show { name } => {
            overview(&d)?;
            let json = d.web(
                "GET",
                &format!("/v1/wireguard/interfaces/{name}/config"),
                None,
            )?;
            print!("{}", text(&json, "config"));
            Ok(())
        }
        WireguardAction::Import(args) => import(&d, args),
        WireguardAction::Peer { action } => peer(&d, action),
    }
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or_default()
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(str::to_string))
        .collect()
}

fn print_log(json: &Value) {
    for line in json["log"].as_array().into_iter().flatten() {
        eprintln!("  {}", line.as_str().unwrap_or_default());
    }
}

/// The overview, or a hint when WireGuard is not installed.
fn overview(d: &Daemon) -> anyhow::Result<Value> {
    let o = d.web("GET", "/v1/wireguard", None)?;
    if !o["installed"].as_bool().unwrap_or(false) {
        anyhow::bail!("{}", t(Msg::WgNotInstalled));
    }
    Ok(o)
}

fn yes(v: &Value) -> &'static str {
    if v.as_bool().unwrap_or(false) {
        "yes"
    } else {
        "no"
    }
}

fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `3m ago`, or `never` for no handshake.
fn ago(unix: i64) -> String {
    if unix <= 0 {
        return "never".to_string();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let secs = (now - unix).max(0);
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

fn status(d: &Daemon) -> anyhow::Result<()> {
    let o = d.web("GET", "/v1/wireguard", None)?;
    if !o["installed"].as_bool().unwrap_or(false) {
        println!("{}", t(Msg::WgNotInstalled));
        return Ok(());
    }
    println!(
        "{:<12} wireguard-tools {}",
        "WIREGUARD",
        text(&o, "version")
    );
    if !text(&o, "primary_address").is_empty() {
        println!("{:<12} {}", "ADDRESS", text(&o, "primary_address"));
    }
    let interfaces = o["interfaces"].as_array().cloned().unwrap_or_default();
    let unmanaged = o["unmanaged"].as_array().cloned().unwrap_or_default();
    if interfaces.is_empty() && unmanaged.is_empty() {
        println!();
        println!("{}", t(Msg::WgNoInterfaces));
        return Ok(());
    }
    for i in &interfaces {
        println!();
        println!(
            "{:<16} {:<5} boot {:<3}  port {:<6}  {}  {}",
            text(i, "name"),
            if i["running"].as_bool().unwrap_or(false) {
                "up"
            } else {
                "down"
            },
            yes(&i["enabled"]),
            match i["listen_port"].as_u64().unwrap_or(0) {
                0 => "-".to_string(),
                p => p.to_string(),
            },
            strings(&i["addresses"]).join(", "),
            if i["masquerade"].as_bool().unwrap_or(false) {
                "NAT"
            } else {
                ""
            },
        );
        let hooks = strings(&i["hooks"]);
        if !hooks.is_empty() {
            println!("  runs as root: {}", hooks.join("; "));
        }
        let peers = i["peers"].as_array().cloned().unwrap_or_default();
        if peers.is_empty() {
            continue;
        }
        println!(
            "  {:<20} {:<7} {:<30} {:<12} {:>10} {:>10}",
            "PEER", "ENABLED", "ALLOWED IPS", "HANDSHAKE", "RX", "TX"
        );
        for p in &peers {
            let live = &p["live"];
            let name = if text(p, "name").is_empty() {
                text(p, "public_key").chars().take(12).collect::<String>() + "…"
            } else {
                text(p, "name").to_string()
            };
            println!(
                "  {:<20} {:<7} {:<30} {:<12} {:>10} {:>10}",
                name,
                yes(&p["enabled"]),
                strings(&p["allowed_ips"]).join(", "),
                if live.is_null() {
                    "-".to_string()
                } else {
                    ago(live["latest_handshake_unix"].as_i64().unwrap_or(0))
                },
                if live.is_null() {
                    "-".to_string()
                } else {
                    bytes(live["rx_bytes"].as_u64().unwrap_or(0))
                },
                if live.is_null() {
                    "-".to_string()
                } else {
                    bytes(live["tx_bytes"].as_u64().unwrap_or(0))
                },
            );
        }
    }
    if !unmanaged.is_empty() {
        println!();
        println!("{}", t(Msg::WgUnmanagedHeader));
        for u in &unmanaged {
            let error = text(u, "error");
            println!(
                "  {:<16} {}{}",
                text(u, "name"),
                if u["running"].as_bool().unwrap_or(false) {
                    "up"
                } else {
                    "down"
                },
                if error.is_empty() {
                    String::new()
                } else {
                    format!("  ({error})")
                }
            );
        }
    }
    Ok(())
}

fn switch(d: &Daemon, name: &str, up: bool) -> anyhow::Result<()> {
    overview(d)?;
    d.web(
        "POST",
        &format!("/v1/wireguard/interfaces/{name}/state"),
        Some(json!({ "up": up })),
    )?;
    println!("{}", tf(if up { Msg::WgUp } else { Msg::WgDown }, name));
    Ok(())
}

fn set(d: &Daemon, args: SetArgs) -> anyhow::Result<()> {
    if args.address.is_empty()
        && args.port.is_none()
        && args.dns.is_empty()
        && args.mtu.is_none()
        && args.endpoint.is_none()
        && args.nat.is_none()
    {
        anyhow::bail!("{}", t(Msg::WgNothingToChange));
    }
    let o = overview(d)?;
    let Some(current) = o["interfaces"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|i| text(i, "name") == args.name)
    else {
        anyhow::bail!(
            "'{}' is not a tunnel the daemon manages (see `asc wireguard status`)",
            args.name
        );
    };
    let mut body = json!({
        "listen_port": current["listen_port"],
        "addresses": current["addresses"],
        "dns": current["dns"],
        "client_dns": current["client_dns"],
        "mtu": current["mtu"],
        "endpoint": current["endpoint"],
        "masquerade": current["masquerade"],
    });
    if !args.address.is_empty() {
        body["addresses"] = json!(args.address);
    }
    if let Some(port) = args.port {
        body["listen_port"] = json!(port);
    }
    if !args.dns.is_empty() {
        body["client_dns"] = json!(args.dns);
    }
    if let Some(mtu) = args.mtu {
        body["mtu"] = json!(mtu);
    }
    if let Some(endpoint) = args.endpoint {
        body["endpoint"] = json!(endpoint);
    }
    if let Some(nat) = args.nat {
        body["masquerade"] = json!(nat);
    }
    // 0 is "not set" in the proto; the REST body takes null for it.
    for key in ["listen_port", "mtu"] {
        if body[key] == json!(0) {
            body[key] = Value::Null;
        }
    }
    d.web(
        "PUT",
        &format!("/v1/wireguard/interfaces/{}", args.name),
        Some(body),
    )?;
    println!("{}", tf(Msg::WgSaved, &args.name));
    Ok(())
}

fn import(d: &Daemon, args: ImportArgs) -> anyhow::Result<()> {
    overview(d)?;
    let (body, default_name) = if args.file == "-" {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        (text, String::new())
    } else {
        let path = std::path::Path::new(&args.file);
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", args.file))?;
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        (text, stem)
    };
    let name = args.name.unwrap_or(default_name);
    if name.is_empty() {
        anyhow::bail!("{}", t(Msg::WgImportNeedsName));
    }
    let done = d.web(
        "POST",
        "/v1/wireguard/import",
        Some(json!({
            "name": name,
            "text": body,
            "overwrite": args.overwrite,
            "accept_hooks": args.accept_hooks,
            "start": args.start,
        })),
    )?;
    for warning in strings(&done["warnings"]) {
        eprintln!("! {warning}");
    }
    println!("{}", tf(Msg::WgImported, text(&done, "name")));
    Ok(())
}

/// Prints a client config, or writes it to `output` readable by its owner only.
fn emit_config(config: &str, output: Option<&str>) -> anyhow::Result<()> {
    match output {
        Some(path) => {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(path)
                .map_err(|e| anyhow::anyhow!("cannot write {path}: {e}"))?;
            file.write_all(config.as_bytes())?;
            eprintln!("{}", tf(Msg::WgConfigSaved, path));
        }
        None => print!("{config}"),
    }
    Ok(())
}

fn peer(d: &Daemon, action: PeerAction) -> anyhow::Result<()> {
    overview(d)?;
    match action {
        PeerAction::Add(args) => {
            let added = d.web(
                "POST",
                "/v1/wireguard/peers",
                Some(json!({
                    "interface": args.interface,
                    "name": args.name,
                    "public_key": args.public_key.unwrap_or_default(),
                    "allowed_ips": args.allowed_ips,
                    "preshared": !args.no_psk,
                    "persistent_keepalive": args.keepalive,
                    "endpoint": args.endpoint.unwrap_or_default(),
                    "client": args.client.json(),
                })),
            )?;
            eprintln!("{}", tf2(Msg::WgPeerAdded, &args.name, &args.interface));
            let config = text(&added, "client_config");
            if config.is_empty() {
                eprintln!("{}", t(Msg::WgNoClientConfig));
            } else {
                eprintln!("{}", t(Msg::WgKeyShownOnce));
                emit_config(config, args.client.output.as_deref())?;
            }
            Ok(())
        }
        PeerAction::Set(args) => peer_set(d, args),
        PeerAction::Remove { interface, peer } => {
            d.web(
                "POST",
                "/v1/wireguard/peers/remove",
                Some(json!({ "interface": interface, "peer": peer })),
            )?;
            println!("{}", tf(Msg::WgPeerRemoved, &peer));
            Ok(())
        }
        PeerAction::Config(args) => {
            let json = d.web(
                "POST",
                "/v1/wireguard/peers/config",
                Some(json!({
                    "interface": args.interface,
                    "peer": args.peer,
                    "client": args.client.json(),
                })),
            )?;
            emit_config(text(&json, "config"), args.client.output.as_deref())
        }
    }
}

fn peer_set(d: &Daemon, args: PeerSetArgs) -> anyhow::Result<()> {
    if args.name.is_none()
        && args.allowed_ips.is_empty()
        && args.endpoint.is_none()
        && args.keepalive.is_none()
        && !args.enable
        && !args.disable
    {
        anyhow::bail!("{}", t(Msg::WgNothingToChange));
    }
    let o = d.web("GET", "/v1/wireguard", None)?;
    let found = o["interfaces"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|i| text(i, "name") == args.interface)
        .and_then(|i| {
            i["peers"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|p| text(p, "public_key") == args.peer || text(p, "name") == args.peer)
        });
    let Some(current) = found else {
        anyhow::bail!(
            "no peer '{}' on {} (give its public key or its name)",
            args.peer,
            args.interface
        );
    };
    let mut body = json!({
        "interface": args.interface,
        "peer": text(current, "public_key"),
        "name": current["name"],
        "allowed_ips": current["allowed_ips"],
        "endpoint": current["endpoint"],
        "persistent_keepalive": current["persistent_keepalive"],
        "enabled": current["enabled"],
    });
    if let Some(name) = args.name {
        body["name"] = json!(name);
    }
    if !args.allowed_ips.is_empty() {
        // `--allowed-ips a,b` and `--allowed-ips a --allowed-ips b` both work.
        let list: Vec<String> = args
            .allowed_ips
            .iter()
            .flat_map(|v| v.split(','))
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .collect();
        body["allowed_ips"] = json!(list);
    }
    if let Some(endpoint) = args.endpoint {
        body["endpoint"] = json!(endpoint);
    }
    if let Some(keepalive) = args.keepalive {
        body["persistent_keepalive"] = json!(keepalive);
    }
    if args.enable {
        body["enabled"] = json!(true);
    }
    if args.disable {
        body["enabled"] = json!(false);
    }
    d.web("PUT", "/v1/wireguard/peers", Some(body))?;
    println!("{}", tf(Msg::WgPeerSaved, &args.peer));
    Ok(())
}
