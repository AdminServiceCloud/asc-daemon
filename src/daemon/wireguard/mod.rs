//! The node's WireGuard: install `wireguard-tools`, manage tunnels
//! as `/etc/wireguard/<name>.conf` files (the ones the daemon wrote carry a
//! marker; other files are listed and left alone), add peers with generated
//! keys and a client config, edit each peer's AllowedIPs, and import a
//! ready-made `.conf`. See docs/english/wireguard.md. Only the root daemon
//! manages WireGuard.
//!
//! The file is the source of truth, so there is no state of our own: every
//! call reads the files, changes the model and writes the file back. Peers
//! and the listen port are applied to a running tunnel with `wg syncconf`;
//! anything `wg-quick` sets up itself (addresses, routes, hooks, NAT) needs
//! the interface rebuilt. A change that WireGuard refuses puts the previous
//! file back.

pub mod conf;
pub mod model;
pub mod tool;

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::daemon::exec::{
    self, Progress, invalid, not_found, precondition, remove_package, run_streaming,
};
use crate::daemon::webserver::write_atomic;
use conf::{ClientOptions, Routes};
use model::{Interface, MARKER, Peer, normalize_nets, valid_name};
use tool::{LivePeer, SystemWg, Wg};

#[derive(Debug, Clone)]
pub struct Paths {
    /// `/etc/wireguard`.
    pub conf_dir: PathBuf,
    /// `/var/lib/asc/wireguard`: scratch space for checking a file before it
    /// replaces the real one.
    pub work: PathBuf,
}

impl Paths {
    pub fn system() -> Self {
        Self {
            conf_dir: PathBuf::from("/etc/wireguard"),
            work: PathBuf::from("/var/lib/asc/wireguard"),
        }
    }

    pub fn conf_file(&self, name: &str) -> PathBuf {
        self.conf_dir.join(format!("{name}.conf"))
    }

    fn backup_file(&self, name: &str) -> PathBuf {
        self.conf_dir.join(format!("{name}.conf.asc-bak"))
    }

    fn candidate(&self, name: &str) -> PathBuf {
        self.work.join("candidate").join(format!("{name}.conf"))
    }
}

// ── What the API shows ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct LiveView {
    pub endpoint: String,
    pub latest_handshake_unix: i64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PeerView {
    #[serde(flatten)]
    pub peer: Peer,
    pub has_preshared_key: bool,
    /// `None` while the interface is down.
    pub live: Option<LiveView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InterfaceView {
    pub name: String,
    pub listen_port: Option<u16>,
    pub addresses: Vec<String>,
    /// What this host uses for DNS while the tunnel is up.
    pub dns: Vec<String>,
    /// What the clients of this server are told to use.
    pub client_dns: Vec<String>,
    pub mtu: Option<u16>,
    pub table: String,
    pub fwmark: String,
    pub endpoint: String,
    pub masquerade: bool,
    pub public_key: String,
    pub running: bool,
    /// Comes up at boot.
    pub enabled: bool,
    /// `PostUp: cmd` lines the operator's hooks run as root.
    pub hooks: Vec<String>,
    pub peers: Vec<PeerView>,
}

/// A file in the config directory the daemon does not manage.
#[derive(Debug, Clone, Serialize)]
pub struct UnmanagedView {
    pub name: String,
    pub running: bool,
    /// Why a file with our marker could not be read; empty for foreign files.
    pub error: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Overview {
    pub installed: bool,
    pub version: String,
    pub interfaces: Vec<InterfaceView>,
    pub unmanaged: Vec<UnmanagedView>,
    /// The node's own address, a starting point for the public endpoint.
    pub primary_address: String,
}

// ── What callers send ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct InterfaceInput {
    pub name: String,
    /// `None` makes a client-style tunnel that does not listen.
    pub listen_port: Option<u16>,
    pub addresses: Vec<String>,
    /// What this host uses for DNS while the tunnel is up (`wg-quick`'s DNS
    /// line); leave empty on a server.
    pub dns: Vec<String>,
    /// What the server's clients are told to use.
    pub client_dns: Vec<String>,
    pub mtu: Option<u16>,
    pub endpoint: String,
    pub masquerade: bool,
    /// Only to bring a key in; empty generates one (new interface) or keeps
    /// the current one (existing).
    pub private_key: String,
}

/// How the client config is built: see [`ClientOptions`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ClientInput {
    /// `full`, `subnet` (or empty) or a comma-separated list of networks.
    pub routes: String,
    pub dns: Vec<String>,
    pub endpoint: String,
}

impl ClientInput {
    fn options(&self) -> Result<ClientOptions> {
        let dns: Vec<String> = self
            .dns
            .iter()
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty())
            .collect();
        Ok(ClientOptions {
            routes: Routes::parse(&self.routes).map_err(invalid)?,
            dns,
            endpoint: self.endpoint.trim().to_string(),
        })
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PeerAdd {
    pub interface: String,
    pub name: String,
    /// Empty: the daemon generates the pair and returns the private key once.
    pub public_key: String,
    /// Empty: the next free address of the interface's network.
    pub allowed_ips: Vec<String>,
    pub preshared: bool,
    pub persistent_keepalive: u16,
    pub endpoint: String,
    pub client: ClientInput,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PeerUpdate {
    pub interface: String,
    /// The peer's public key, or its name when that is unique.
    pub peer: String,
    pub name: String,
    pub allowed_ips: Vec<String>,
    pub endpoint: String,
    pub persistent_keepalive: u16,
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ImportInput {
    pub name: String,
    pub text: String,
    /// Replace an interface of that name (the old file is kept as `.asc-bak`).
    pub overwrite: bool,
    /// The file may run PreUp/PostUp/PreDown/PostDown commands as root.
    pub accept_hooks: bool,
    /// Bring the tunnel up and enable it at boot.
    pub start: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Added {
    pub overview: Overview,
    pub public_key: String,
    /// The client's config; empty for a peer that only routes networks. It
    /// holds the private key only when the daemon generated the pair.
    pub client_config: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Imported {
    pub overview: Overview,
    pub name: String,
    /// What the file had that was dropped or not understood.
    pub warnings: Vec<String>,
}

// ── The manager ─────────────────────────────────────────────────────────────

pub struct Wireguard {
    paths: Paths,
    wg: Box<dyn Wg>,
    lock: Mutex<()>,
}

impl Wireguard {
    pub fn new() -> Self {
        Self::with(Paths::system(), Box::new(SystemWg))
    }

    pub fn with(paths: Paths, wg: Box<dyn Wg>) -> Self {
        Self {
            paths,
            wg,
            lock: Mutex::new(()),
        }
    }

    fn guard(&self) -> MutexGuard<'_, ()> {
        self.lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn require_installed(&self) -> Result<()> {
        if self.wg.installed() {
            Ok(())
        } else {
            Err(precondition(
                "WireGuard is not installed on this node (run `asc wireguard install`)",
            ))
        }
    }

    // ── Reading ─────────────────────────────────────────────────────────────

    pub fn overview(&self) -> Result<Overview> {
        let _guard = self.guard();
        Ok(self.overview_locked())
    }

    /// The `*.conf` names in the config directory, sorted.
    fn conf_names(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.paths.conf_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let file = entry.file_name().into_string().ok()?;
                let name = file.strip_suffix(".conf")?;
                valid_name(name).then(|| name.to_string())
            })
            .collect();
        names.sort();
        names
    }

    fn overview_locked(&self) -> Overview {
        let installed = self.wg.installed();
        let running = if installed {
            self.wg.running().unwrap_or_default()
        } else {
            Vec::new()
        };
        let mut interfaces = Vec::new();
        let mut unmanaged = Vec::new();
        for name in self.conf_names() {
            let up = running.contains(&name);
            let text = std::fs::read_to_string(self.paths.conf_file(&name)).unwrap_or_default();
            let ours = text
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .is_some_and(|l| l.starts_with(MARKER));
            if !ours {
                unmanaged.push(UnmanagedView {
                    name,
                    running: up,
                    error: String::new(),
                });
                continue;
            }
            match conf::parse(&name, &text) {
                Ok(parsed) => interfaces.push(self.view(parsed.interface, up)),
                Err(err) => unmanaged.push(UnmanagedView {
                    name,
                    running: up,
                    error: format!("{err:#}"),
                }),
            }
        }
        Overview {
            installed,
            version: if installed {
                self.wg.version()
            } else {
                String::new()
            },
            interfaces,
            unmanaged,
            primary_address: if installed {
                self.wg.primary_address()
            } else {
                String::new()
            },
        }
    }

    fn view(&self, iface: Interface, running: bool) -> InterfaceView {
        let live: Vec<LivePeer> = if running {
            self.wg.live_peers(&iface.name).unwrap_or_default()
        } else {
            Vec::new()
        };
        let peers = iface
            .peers
            .iter()
            .map(|peer| PeerView {
                has_preshared_key: !peer.preshared_key.is_empty(),
                live: live
                    .iter()
                    .find(|l| l.public_key == peer.public_key)
                    .map(|l| LiveView {
                        endpoint: l.endpoint.clone(),
                        latest_handshake_unix: l.latest_handshake_unix,
                        rx_bytes: l.rx_bytes,
                        tx_bytes: l.tx_bytes,
                    }),
                peer: peer.clone(),
            })
            .collect();
        InterfaceView {
            public_key: self.wg.pubkey(&iface.private_key).unwrap_or_default(),
            running,
            enabled: self.wg.enabled(&iface.name),
            hooks: iface.hooks.lines(),
            name: iface.name,
            listen_port: iface.listen_port,
            addresses: iface.addresses,
            dns: iface.dns,
            client_dns: iface.client_dns,
            mtu: iface.mtu,
            table: iface.table,
            fwmark: iface.fwmark,
            endpoint: iface.endpoint,
            masquerade: iface.masquerade,
            peers,
        }
    }

    /// The interface of a file the daemon manages; `None` when there is no
    /// such file. A foreign file of that name is an error.
    fn read_managed_opt(&self, name: &str) -> Result<Option<Interface>> {
        if !valid_name(name) {
            return Err(invalid(format!("'{name}' is not a usable interface name")));
        }
        let path = self.paths.conf_file(name);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("cannot read {}", path.display())),
        };
        let ours = text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .is_some_and(|l| l.starts_with(MARKER));
        if !ours {
            return Err(precondition(format!(
                "{name} is not managed by ASC; import it (with overwrite) to take it over"
            )));
        }
        Ok(Some(
            conf::parse(name, &text)
                .map_err(|e| precondition(format!("cannot read {}: {e:#}", path.display())))?
                .interface,
        ))
    }

    fn read_managed(&self, name: &str) -> Result<Interface> {
        self.read_managed_opt(name)?
            .ok_or_else(|| not_found(format!("there is no interface '{name}'")))
    }

    // ── Installing ──────────────────────────────────────────────────────────

    pub fn install(&self, progress: Progress<'_>) -> Result<Overview> {
        let _guard = self.guard();
        if self.wg.installed() {
            progress("WireGuard tools are already installed");
        } else {
            exec::install_package("wireguard-tools", progress)?;
            if !self.wg.installed() {
                return Err(precondition(
                    "the wireguard-tools package installed, but `wg` and `wg-quick` are not on PATH",
                ));
            }
        }
        // Built into Linux 5.6+; older kernels load a module.
        if run_streaming("modprobe", &["wireguard"], progress).is_err() {
            progress(
                "note: the wireguard kernel module did not load; on a kernel older than 5.6 install wireguard-dkms",
            );
        }
        Ok(self.overview_locked())
    }

    pub fn uninstall(&self, purge: bool, progress: Progress<'_>) -> Result<()> {
        let _guard = self.guard();
        self.require_installed()?;
        for name in self.conf_names() {
            if self.read_managed_opt(&name).ok().flatten().is_some() {
                progress(&format!("stopping {name}"));
                let _ = self.wg.down(&name);
            }
        }
        remove_package("wireguard-tools", purge, progress)?;
        if purge {
            for name in self.conf_names() {
                if self.read_managed_opt(&name).ok().flatten().is_some() {
                    let _ = std::fs::remove_file(self.paths.conf_file(&name));
                    let _ = std::fs::remove_file(self.paths.backup_file(&name));
                }
            }
            let _ = std::fs::remove_dir_all(&self.paths.work);
        }
        Ok(())
    }

    // ── Applying ────────────────────────────────────────────────────────────

    /// Checks the new file, writes it and brings a running interface in line
    /// (`wg syncconf`, or a rebuild when `wg-quick`'s own settings changed).
    /// If that fails the previous file is put back.
    fn write_and_sync(&self, old: Option<&Interface>, new: &Interface) -> Result<()> {
        let name = &new.name;
        let text = conf::render(new);
        let candidate = self.paths.candidate(name);
        write_atomic(&candidate, text.as_bytes(), 0o600)
            .context("cannot write the candidate configuration")?;
        let checked = self.wg.strip(&candidate);
        let _ = std::fs::remove_file(&candidate);
        checked.map_err(|e| invalid(format!("WireGuard rejects this configuration: {e:#}")))?;

        let path = self.paths.conf_file(name);
        let previous = std::fs::read(&path).ok();
        write_atomic(&path, text.as_bytes(), 0o600).context("cannot write the configuration")?;
        if !self.wg.running()?.iter().any(|n| n == name) {
            return Ok(());
        }
        let applied = if old.is_none_or(|o| needs_rebuild(o, new)) {
            self.wg.restart(name)
        } else {
            self.wg
                .strip(&path)
                .and_then(|stripped| self.wg.syncconf(name, &stripped))
        };
        if let Err(err) = applied {
            match &previous {
                Some(bytes) => {
                    let _ = write_atomic(&path, bytes, 0o600);
                }
                None => {
                    let _ = std::fs::remove_file(&path);
                }
            }
            // The tunnel may have been torn down half-way; bring it back as it was.
            let _ = self.wg.restart(name);
            return Err(precondition(format!(
                "WireGuard could not apply the change, the previous configuration was restored: {err:#}"
            )));
        }
        Ok(())
    }

    /// No two managed interfaces may listen on one port.
    fn check_port(&self, iface: &Interface) -> Result<()> {
        let Some(port) = iface.listen_port else {
            return Ok(());
        };
        for name in self.conf_names() {
            if name == iface.name {
                continue;
            }
            if let Ok(Some(other)) = self.read_managed_opt(&name)
                && other.listen_port == Some(port)
            {
                return Err(precondition(format!(
                    "port {port} is already used by the interface {name}"
                )));
            }
        }
        Ok(())
    }

    /// Reads a managed interface, lets `edit` change it, and applies the result.
    fn modify<T>(
        &self,
        name: &str,
        edit: impl FnOnce(&mut Interface) -> Result<T>,
    ) -> Result<(Interface, T)> {
        self.require_installed()?;
        let old = self.read_managed(name)?;
        let mut new = old.clone();
        let out = edit(&mut new)?;
        new.normalize().map_err(invalid)?;
        self.write_and_sync(Some(&old), &new)?;
        Ok((new, out))
    }

    // ── Interfaces ──────────────────────────────────────────────────────────

    pub fn upsert_interface(&self, input: InterfaceInput) -> Result<Overview> {
        let _guard = self.guard();
        self.require_installed()?;
        let existing = self.read_managed_opt(&input.name)?;
        let mut new = match &existing {
            Some(old) => old.clone(),
            None => {
                let key = if input.private_key.trim().is_empty() {
                    self.wg.genkey()?
                } else {
                    input.private_key.trim().to_string()
                };
                Interface::new(&input.name, &key)
            }
        };
        if existing.is_some() && !input.private_key.trim().is_empty() {
            new.private_key = input.private_key.trim().to_string();
        }
        new.listen_port = input.listen_port;
        new.addresses = input.addresses;
        new.dns = input.dns;
        new.client_dns = input.client_dns;
        new.mtu = input.mtu;
        new.endpoint = input.endpoint;
        new.masquerade = input.masquerade;
        new.normalize().map_err(invalid)?;
        self.check_port(&new)?;
        self.write_and_sync(existing.as_ref(), &new)?;
        if existing.is_none()
            && let Err(err) = self.wg.up(&new.name)
        {
            let _ = std::fs::remove_file(self.paths.conf_file(&new.name));
            return Err(precondition(format!(
                "the interface {} did not come up: {err:#}",
                new.name
            )));
        }
        Ok(self.overview_locked())
    }

    pub fn remove_interface(&self, name: &str) -> Result<Overview> {
        let _guard = self.guard();
        self.read_managed(name)?;
        self.wg.down(name)?;
        let _ = std::fs::remove_file(self.paths.conf_file(name));
        Ok(self.overview_locked())
    }

    /// Brings an interface up (and enables it at boot) or takes it down. Works
    /// on files the daemon does not manage too: it never edits them.
    pub fn set_state(&self, name: &str, up: bool) -> Result<Overview> {
        let _guard = self.guard();
        self.require_installed()?;
        self.require_exists(name)?;
        if up {
            self.wg.up(name)?;
        } else {
            self.wg.down(name)?;
        }
        Ok(self.overview_locked())
    }

    fn require_exists(&self, name: &str) -> Result<()> {
        if self.conf_names().iter().any(|n| n == name) {
            Ok(())
        } else {
            Err(not_found(format!("there is no interface '{name}'")))
        }
    }

    /// The interface's file as it is on disk, with its private and pre-shared
    /// keys hidden — to look at, for the daemon's files and foreign ones alike.
    pub fn interface_config(&self, name: &str) -> Result<String> {
        let _guard = self.guard();
        if !valid_name(name) {
            return Err(invalid(format!("'{name}' is not a usable interface name")));
        }
        self.require_exists(name)?;
        let path = self.paths.conf_file(name);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        Ok(conf::redact(&text))
    }

    // ── Import ──────────────────────────────────────────────────────────────

    pub fn import(&self, input: ImportInput) -> Result<Imported> {
        let _guard = self.guard();
        self.require_installed()?;
        let name = input.name.trim().to_string();
        if !valid_name(&name) {
            return Err(invalid(format!(
                "'{name}' is not a usable interface name; give one with the name option"
            )));
        }
        let parsed = conf::parse(&name, &input.text).map_err(invalid)?;
        let mut iface = parsed.interface;
        if !iface.hooks.is_empty() && !input.accept_hooks {
            return Err(precondition(format!(
                "the file runs commands as root when the tunnel goes up or down ({}); review them and acknowledge to import it",
                iface.hooks.lines().join("; ")
            )));
        }
        iface.normalize().map_err(invalid)?;
        self.check_port(&iface)?;
        let path = self.paths.conf_file(&name);
        if path.exists() {
            if !input.overwrite {
                return Err(precondition(format!(
                    "{name} already exists; choose another name or overwrite it"
                )));
            }
            let _ = std::fs::copy(&path, self.paths.backup_file(&name));
        }
        self.write_and_sync(None, &iface)?;
        if input.start
            && let Err(err) = self.wg.up(&name)
        {
            return Err(precondition(format!(
                "the file was imported but {name} did not come up: {err:#}"
            )));
        }
        Ok(Imported {
            overview: self.overview_locked(),
            name,
            warnings: parsed.warnings,
        })
    }

    // ── Peers ───────────────────────────────────────────────────────────────

    pub fn add_peer(&self, add: PeerAdd) -> Result<Added> {
        let options = add.client.options()?;
        let _guard = self.guard();
        let (new, (public_key, private_key)) = self.modify(&add.interface, |iface| {
            let (public_key, private_key) = if add.public_key.trim().is_empty() {
                let private = self.wg.genkey()?;
                (self.wg.pubkey(&private)?, Some(private))
            } else {
                (add.public_key.trim().to_string(), None)
            };
            let mut peer = Peer::new(&public_key);
            peer.name = add.name.clone();
            peer.endpoint = add.endpoint.clone();
            peer.persistent_keepalive = add.persistent_keepalive;
            if add.preshared {
                peer.preshared_key = self.wg.genpsk()?;
            }
            peer.allowed_ips = if add.allowed_ips.is_empty() {
                iface.free_addresses().map_err(precondition)?
            } else {
                normalize_nets(&add.allowed_ips).map_err(invalid)?
            };
            iface.peers.push(peer);
            Ok((public_key, private_key))
        })?;
        let peer = new
            .peers
            .iter()
            .find(|p| p.public_key == public_key)
            .expect("the peer was just added");
        let server_key = self.wg.pubkey(&new.private_key)?;
        // A peer that only routes networks has no address to put in a client config.
        let client_config = conf::client_config(
            &new,
            &server_key,
            peer,
            private_key.as_deref(),
            &options,
            &self.wg.primary_address(),
        )
        .unwrap_or_default();
        Ok(Added {
            overview: self.overview_locked(),
            public_key,
            client_config,
        })
    }

    pub fn update_peer(&self, update: PeerUpdate) -> Result<Overview> {
        let _guard = self.guard();
        self.modify(&update.interface, |iface| {
            let index = iface.peer_index(&update.peer)?;
            let allowed = normalize_nets(&update.allowed_ips).map_err(invalid)?;
            if allowed.is_empty() {
                return Err(invalid(
                    "a peer needs at least one AllowedIPs entry (its address or the networks behind it)",
                ));
            }
            let peer = &mut iface.peers[index];
            peer.name = update.name.clone();
            peer.allowed_ips = allowed;
            peer.endpoint = update.endpoint.clone();
            peer.persistent_keepalive = update.persistent_keepalive;
            peer.enabled = update.enabled;
            Ok(())
        })?;
        Ok(self.overview_locked())
    }

    pub fn remove_peer(&self, interface: &str, peer: &str) -> Result<Overview> {
        let _guard = self.guard();
        self.modify(interface, |iface| {
            let index = iface.peer_index(peer)?;
            iface.peers.remove(index);
            Ok(())
        })?;
        Ok(self.overview_locked())
    }

    /// The client config of an existing peer. The daemon does not keep the
    /// peer's private key, so the file carries a placeholder for it.
    pub fn peer_config(&self, interface: &str, peer: &str, client: &ClientInput) -> Result<String> {
        let options = client.options()?;
        let _guard = self.guard();
        self.require_installed()?;
        let iface = self.read_managed(interface)?;
        let index = iface.peer_index(peer)?;
        conf::client_config(
            &iface,
            &self.wg.pubkey(&iface.private_key)?,
            &iface.peers[index],
            None,
            &options,
            &self.wg.primary_address(),
        )
        .map_err(precondition)
    }
}

impl Default for Wireguard {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether going from `old` to `new` needs `wg-quick` to rebuild the
/// interface: it owns the addresses, routes, DNS and hooks, and `wg
/// syncconf` touches none of them.
fn needs_rebuild(old: &Interface, new: &Interface) -> bool {
    old.addresses != new.addresses
        || old.mtu != new.mtu
        || old.dns != new.dns
        || old.table != new.table
        || old.fwmark != new.fwmark
        || old.hooks != new.hooks
        || old.masquerade != new.masquerade
        || old.extra_routes() != new.extra_routes()
}

#[cfg(test)]
mod tests;
