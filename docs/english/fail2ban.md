# 🚫 fail2ban (daemon)

> 🌍 **Language:** English · [🇷🇺 Русская версия](../russian/fail2ban.md)

## 📌 Description

The `fail2ban` module installs [fail2ban](https://github.com/fail2ban/fail2ban) on the node and manages it: it bans addresses that keep failing to log in to SSH, hit the web server with scanners or repeat their offences. Bans are enforced through **nftables** in fail2ban's own table, `inet f2b-table`, next to the daemon's `inet asc` ([🛡️ firewall](firewall.md)), so the two never overwrite each other.

The daemon owns exactly one file, `/etc/fail2ban/jail.d/asc.local`. The operator's `jail.local` and the other `jail.d` files are never modified. Everything works standalone through `asc fail2ban …`; the AdminService.Cloud platform uses the same API ([🧩 node-modules](../../../asc-platform/docs/features/node-modules.md)). The command needs the running daemon. Only the system (root) daemon manages fail2ban.

## 🎯 Scenarios

- 🧰 `sudo asc fail2ban install` installs the package (apt / dnf), enables the service, writes the configuration and starts the `sshd` jail; the log is streamed.
- 🔑 Five failed SSH logins in ten minutes ban the address for an hour (the defaults); the ban grows for repeat offenders. `asc fail2ban enable recidive` adds the jail that catches addresses banned again and again and bans them for longer, on every port.
- 🌐 With the [web server](webserver.md) installed, the `nginx-http-auth`, `nginx-botsearch` and `nginx-limit-req` jails can be switched on; they read the logs the daemon's own nginx writes under `/var/log/asc/webserver/`.
- 🛡️ `asc fail2ban settings --add-ignore 203.0.113.0/24` keeps the office network from ever being banned.
- 🎛️ `asc fail2ban tune sshd --maxretry 3 --bantime 1d` overrides one jail; `asc fail2ban settings --bantime -1` bans for good.
- 🔓 `asc fail2ban unban 198.51.100.4` releases an address that a colleague locked out by mistake; `asc fail2ban ban 198.51.100.9 --jail sshd` bans one by hand; `asc fail2ban bans` shows who is banned and until when.
- 🧪 A broken override is rejected: the file is checked with `fail2ban-client -t` before it is loaded and the previous version stays.

## 🏗️ Technical design

Code: `src/daemon/fail2ban/` — `model.rs` (settings, jails, the catalog, validation), `config.rs` (renders `asc.local`), `client.rs` (the `fail2ban-client` wrapper and the parsers of its output), `mod.rs` (the manager).

### Install and removal

`InstallFail2banStream` streams `log | done | error`, like the web server install: `apt-get install fail2ban` or `dnf install fail2ban`, then the configuration is written and the service is enabled and started. `UninstallFail2ban(purge)` stops the service and removes the package, and `asc.local`; with `purge` also the daemon's state.

### Configuration

- **Defaults** (`[DEFAULT]`): `bantime` (`1h`), `findtime` (`10m`), `maxretry` (`5`), `bantime.increment` (a growing ban for repeat offenders), `ignoreip` (loopback plus the operator's list), and `banaction = nftables-multiport` / `banaction_allports = nftables-allports`. Times use fail2ban's syntax: `90`, `10m`, `1h`, `1d`, `1w`; `-1` bans for good.
- **Jails**: `sshd` (on by default), `recidive`, `nginx-http-auth`, `nginx-botsearch`, `nginx-limit-req`. For each: `enabled`, `maxretry`, `bantime`, `findtime`, `port`, `logpath`. The nginx jails exist only while the `webserver` module is installed; their log paths default to the web server's.
- Everything the operator can type is validated (times, numbers, port lists, absolute log paths, addresses) so a value cannot start a new config line.
- Every change goes through the same steps: render, write (keeping the previous file), `fail2ban-client -t`, and — if fail2ban refuses — put the previous file back and keep the previous model. Then `fail2ban-client reload`.
- **A reload can leave a jail without a ban action** when the action it used is replaced (fresh install: the stock config bans with `nftables`, ours with `nftables-multiport`) — the jail then counts failures and bans nobody. After every reload the daemon checks the jails it configures and restarts fail2ban if one lost its action; if even that does not help, the problem is reported in `last_error` instead of being hidden. Bans survive a restart: fail2ban restores them from its own database.

### Management

Through the official client: `status`, `status <jail>`, `get <jail> banip --with-time`, `get <jail> actions`, `set <jail> banip|unbanip <ip>`. The output is parsed into typed messages; an unexpected format becomes an error, never a silent empty list. `fail2ban-client` is a Python start per call, so jail statuses are reused for 4 s and dropped after any change. A ban runs its nftables action asynchronously inside fail2ban — it is in force about a second later.

### API (`Fail2banService`)

| RPC | What it does |
|---|---|
| `GetFail2ban` | Installed, running, version, defaults, every known jail with its overrides, whether it is available and running, and its counters |
| `InstallFail2banStream`, `UninstallFail2ban` | Installation with a log stream, removal |
| `UpdateFail2banSettings`, `UpsertFail2banJail` | Edit `asc.local` |
| `ListFail2banBans` | Banned addresses: IP, jail, since when, until when |
| `BanFail2banIp`, `UnbanFail2banIp` | Manual ban and release (a single address, not a network) |

Capability in `GetStatus`: `fail2ban`. All calls are root-only. The same operations are available as REST under `/v1/fail2ban` for the CLI.

### CLI

`asc fail2ban status | install | uninstall | settings | jails | enable | disable | tune | bans | ban | unban`. Command reference: <https://docs.adminservice.cloud/commands/fail2ban>.

## 🔗 Related tasks

| ID | What |
|---|---|
| DMN-150 | Module: install, configuration, jails, bans |
| DMN-151 | CLI, translations, documentation mirror |

See also: [🛡️ firewall](firewall.md), [🌐 webserver](webserver.md).
