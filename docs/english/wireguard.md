# 🔐 WireGuard (daemon)

> 🌍 **Language:** English · [🇷🇺 Русская версия](../russian/wireguard.md)

## 📌 Description

The `wireguard` module installs [WireGuard](https://www.wireguard.com/) on the node and manages its tunnels: a VPN server for your team, a site-to-site link between two servers, or a client tunnel to someone else's VPN. It creates interfaces, hands out addresses and keys to peers, builds ready-to-use client configs, shows live handshakes and traffic, and **imports an existing `.conf`** — yours or one a provider gave you.

The source of truth is the standard `/etc/wireguard/<name>.conf` read by `wg-quick`, so a tunnel keeps working without the daemon and can be handed over to other tools. The daemon manages the files that carry its marker (`# Managed by ASC`); any other `*.conf` in that directory is listed, can be read (keys hidden) and switched on and off, and is never edited — import it to take it over. Everything works standalone through `asc wireguard …`; the AdminService.Cloud platform uses the same API ([🧩 node-modules](../../../asc-platform/docs/features/node-modules.md)). The command needs the running daemon. Only the system (root) daemon manages WireGuard.

## 🎯 Scenarios

- 🧰 `sudo asc wireguard install` installs `wireguard-tools` (apt / dnf); the log is streamed.
- 🛰️ `sudo asc wireguard add wg0 --address 10.8.0.1/24 --port 51820 --endpoint vpn.example.com --nat` creates a VPN server: a fresh key pair, the interface up now and at boot, and, with `--nat`, the host masquerades the VPN subnet to the internet.
- 📱 `sudo asc wireguard peer add wg0 phone` takes the next free address from the subnet, generates the keys and a pre-shared key, and prints the client config (the private key is shown **once**; the daemon stores only the public key). The platform shows it as text, a download and a QR code.
- 🧭 `--routes` decides what the client sends through the tunnel — its **AllowedIPs**: `full` (`0.0.0.0/0, ::/0`, everything), `subnet` (only the VPN network, the default) or a list such as `10.8.0.0/24,192.168.10.0/24`. On the server side `peer set wg0 <key> --allowed-ips 10.8.0.2/32,192.168.50.0/24` makes a peer also reach a network behind it (site-to-site).
- 📥 `sudo asc wireguard import office.conf` takes a ready file. A **server** file (with `ListenPort` and peers) and a **client** file (one peer with an `Endpoint` and `AllowedIPs = 0.0.0.0/0`) both work: the interface is validated, rewritten with the ASC marker, its keys are kept and it is brought up with `--start`. The name defaults to the file name.
- 🔁 `peer set wg0 <key> --disable` cuts a lost phone off without forgetting its entry; `--enable` brings it back. `peer remove` forgets it.
- 📊 `asc wireguard status` shows, per interface, the peers with the last handshake and the traffic each moved. `asc wireguard show wg0` prints the interface's file with its keys hidden; `up` / `down` switch a tunnel on and off (and at boot). The panel does the same from the list: click a tunnel to read its configuration and flip its switch, click a peer to see its details and switch it.
- ⚠️ A file with `PostUp` / `PostDown` / `PreUp` / `PreDown` runs commands as root; it is refused until you pass `--accept-hooks`. `SaveConfig = true` is dropped (it would let `wg-quick` overwrite the file).

## 🏗️ Technical design

Code: `src/daemon/wireguard/` — `model.rs` (interface, peer, validation, address allocation), `conf.rs` (the `.conf` parser and renderer, the client config), `tool.rs` (the `wg` / `wg-quick` / systemd wrapper behind a trait, the `wg show dump` parser), `mod.rs` (the manager).

### Install and removal
`install` runs the package manager (apt, dnf or yum) for `wireguard-tools`; the kernel module is part of Linux 5.6+ (older kernels need `wireguard-dkms`, which the daemon does not install — it says so). `uninstall` stops the managed interfaces and removes the package; `--purge` also deletes the managed `.conf` files (which hold the private keys).

### The file
```
# Managed by ASC. Edit with `asc wireguard` or the panel; manual edits are kept
# only while the format below stays intact.
[Interface]
# asc:endpoint = vpn.example.com
# asc:client-dns = 1.1.1.1
# asc:masquerade
PrivateKey = …
Address = 10.8.0.1/24
ListenPort = 51820
PostUp = … # asc:nat
PostDown = … # asc:nat

[Peer]
# asc:name = phone
PublicKey = …
PresharedKey = …
AllowedIPs = 10.8.0.2/32
PersistentKeepalive = 25
```
- `# asc:` comments carry what WireGuard has no field for (the public address and the DNS for client configs, the peer's name, the NAT switch). `wg` and `wg-quick` ignore comments.
- The DNS handed to **clients** is deliberately not a `DNS =` line: on a server `wg-quick` would apply that line to the server itself (through `resolvconf`) and change how the host resolves names. A real `DNS =` is kept only where it belongs — in an imported client file, which is a tunnel for this host's own traffic.
- A **disabled** peer is kept as a `[Peer]` block whose every line starts with `#asc:off `; it is not loaded.
- With NAT on, the daemon writes the two hook lines tagged `# asc:nat`: `net.ipv4.ip_forward=1` and a private nftables table `ip asc_wg_<name>` with one `masquerade` rule for the VPN subnet. They are removed when NAT is switched off; other hooks are never touched. The firewall module ([🛡️ firewall](firewall.md)) and this table are separate, so neither overwrites the other.
- The file is written atomically with mode `0600` and checked with `wg-quick strip` before it is used.

### Applying changes
- **Peers and the listen port** change a running tunnel without dropping it: `wg syncconf`.
- **Addresses, MTU, DNS, routing table, hooks, NAT** need `wg-quick` to rebuild the interface: the daemon restarts it (a brief gap).
- If the new file is refused, the previous one is put back and the interface is restored.
- An interface is brought up and enabled at boot with `systemctl enable --now wg-quick@<name>` (`wg-quick up` where there is no systemd). If `wg-quick` refuses the file, the unit is not left enabled and the error carries the last lines of its journal.

### Addresses and AllowedIPs
- A new peer gets the next free host address of the interface's network (`/32` for IPv4, `/128` for IPv6), skipping the server's own and every address already in a peer's AllowedIPs. You can also give the addresses yourself.
- **AllowedIPs of a peer on the server** are the addresses and networks routed to that peer; the daemon checks that they are valid CIDRs and that a host address is not claimed by two peers on one interface.
- **AllowedIPs of the client config** are what the client sends through the tunnel; they are an option of the config (`full`, `subnet` or a list), not stored.
- The client `Address` is the peer's host addresses.

### Import
`import` parses the text with the same parser as the managed files, so a file the daemon wrote and a file from elsewhere take the same path:
- required: `[Interface]` with a valid `PrivateKey`; every peer needs a `PublicKey`; keys must be 32 bytes of base64; addresses, `AllowedIPs`, `Endpoint`, ports and `MTU` are validated;
- kept: `Address`, `ListenPort`, `DNS`, `MTU`, `Table`, `FwMark`, peers with their `PresharedKey`, `Endpoint`, `PersistentKeepalive`, and the hooks (only with the acknowledgement);
- dropped, with a warning in the result: `SaveConfig`, unknown keys;
- an existing interface of the same name is refused unless `--overwrite` is given; the old file is saved as `<name>.conf.asc-bak`.

### Errors
Invalid input → `INVALID_ARGUMENT`; WireGuard not installed, the interface is not managed by ASC, a name or port already used → `FAILED_PRECONDITION`; unknown interface or peer → `NOT_FOUND`.

### API (`WireguardService`)
- `GetWireguard` — installed, version, every managed interface with its peers and live data (handshake time, endpoint, bytes in and out), the unmanaged ones (name and state), the node's primary address (a hint for the public endpoint). Private and pre-shared keys are **never** returned; a pre-shared key appears only inside a client config.
- `GetWireguardInterfaceConfig` — the interface's file for reading, private and pre-shared keys replaced by `(hidden)`. Works for unmanaged files too.
- `InstallWireguardStream` (log | done | error), `UninstallWireguard`.
- `UpsertWireguardInterface` — create or change: name, listen port, addresses, client DNS (and the host's own `dns` for a client-style tunnel), MTU, public endpoint, NAT.
- `RemoveWireguardInterface`, `SetWireguardInterfaceState` (up and enable at boot / down and disable; also for unmanaged files).
- `ImportWireguardConfig` — the text, `overwrite`, `accept_hooks`, `start`; returns the state and the warnings.
- `AddWireguardPeer` — returns the client config (with the private key when the daemon generated it); `UpdateWireguardPeer`, `RemoveWireguardPeer`.
- `GetWireguardPeerConfig` — the client config of an existing peer with a `<PRIVATE_KEY>` placeholder (the daemon does not keep it).

Capability: `wireguard`. The REST routes under `/v1/wireguard` mirror the calls for the CLI.

### CLI
`asc wireguard status | install | uninstall | add | set | remove | up | down | show | import | peer add | peer set | peer remove | peer config`. Strings go through the translation system (EN, RU). Command reference: <https://docs.adminservice.cloud/commands/wireguard>.

## 🔗 Related tasks

[DMN-152](../../../asc-platform/ROADMAP.md) (module), [DMN-153](../../../asc-platform/ROADMAP.md) (CLI, docs). Platform side: NODE-073, BE-093, FE-268 — [🧩 node-modules](../../../asc-platform/docs/features/node-modules.md).
