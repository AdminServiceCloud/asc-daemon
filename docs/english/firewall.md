# 🛡️ Firewall (daemon)

> 🌍 **Language:** English · [🇷🇺 Русская версия](../russian/firewall.md)

## 📌 Description

The `firewall` module manages the host firewall through **nftables**. The daemon owns exactly one table, `inet asc`, and never touches anything else: Docker's, fail2ban's and the operator's own tables stay as they are (the daemon only lists them, read-only).

Every change is applied with an **automatic rollback**: the daemon keeps what undoes the change and runs it unless the change is confirmed within a deadline. A rule that cuts off the daemon's API or SSH therefore cannot lock the operator out for good — and neither can a lost connection, because confirming needs a working connection.

For experts there is a **RAW mode**: the whole ruleset of the server is edited as text. It is deliberately fenced off — it needs an explicit risk acknowledgement and uses the same rollback.

Everything works standalone through `asc firewall …`; the AdminService.Cloud platform uses the same API (🧩 node-modules). The command needs the running daemon, because the rollback timer lives inside it. Only the system (root) daemon manages the firewall.

## 🎯 Scenarios

- 🔒 `sudo asc firewall enable` loads `table inet asc` with policy `drop` for inbound traffic plus the SSH, daemon-API and (when the web server is installed) HTTP/HTTPS rules the daemon derives from the host, then asks to confirm within 60 s.
- 🌐 `asc firewall allow 80,443/tcp` opens the web ports; `asc firewall allow 5432/tcp --from 10.0.0.0/8` opens Postgres for a private network only. With the firewall on, a rule change is applied at once (with the same confirmation window); `--no-apply` only stores it.
- 🚫 `asc firewall deny --from 203.0.113.7` drops everything from one address; `asc firewall set add blocklist 203.0.113.7 --ttl 24h` does it for a day and forgets it afterwards. The blocklist also guards Docker's published ports.
- 🐳 A container publishes `-p 8080:80`. Docker's NAT rules bypass the `input` chain, so a rule with scope `docker` is matched in the `forward` chain by the original destination port: `asc firewall deny 8080/tcp --scope docker --from 203.0.113.0/24` really closes the published port. `asc firewall settings --protect-docker true` closes all of them unless a rule opens them.
- ⏱️ The operator applies a change over SSH and loses the connection. Nobody confirms, the deadline passes, the previous rules are restored — the connection comes back. After a rollback the edit stays stored: `asc firewall status` says there are unapplied changes.
- 🧑‍🔬 `asc firewall ruleset edit --i-understand-the-risk` opens the full `nft list ruleset` in `$EDITOR`. It is checked with `nft -c`, applied with the same rollback.
- ⚠️ ufw or firewalld is active on the host. `asc firewall enable` refuses and says why; `--force` switches the other tool off, it never merges with it.

## 🏗️ Technical design

Code: `src/daemon/firewall/`:

| File | What it does |
|---|---|
| `model.rs` | Settings, rules, IP sets, validation (ports, CIDR, protocol, comments) |
| `host.rs` | Host facts (SSH ports from `sshd -T`, the API port, web server installed) and the presets derived from them |
| `render.rs` | Model → text of `table inet asc`; deterministic, a counter on every rule |
| `nft.rs` | The `nft` binary behind a trait: `-c` check, `-f` apply, listing, counters from `-j` |
| `mod.rs` | The manager: stored model, apply, confirm, rollback, timer, recovery after a restart |
| `persist.rs` | `/etc/asc/firewall/ruleset.nft` and the `asc-firewall.service` unit |
| `conflicts.rs` | Detects active ufw and firewalld |

### Modes

| Mode | Meaning |
|---|---|
| `disabled` | The daemon touches nothing; `table inet asc` is removed |
| `managed` | The daemon owns `table inet asc`, built from the settings and rules below |
| `raw` | The ruleset is the operator's text; the managed model is stored but not applied |

### Managed model

- **Settings**: `input_policy` (`accept` / `drop`), `allow_icmp`, `ipv6` (off leaves IPv6 unfiltered), `protect_docker`, `disabled_presets`.
- **Rule**: `id`, `enabled`, `action` (`accept` / `drop` / `reject`), `protocol` (`tcp` / `udp` / `icmp` / `any`), `ports` (single ports and ranges, tcp and udp only), `sources` (IPs, CIDRs, `@allowlist`, `@blocklist`; empty is any), `scope` (`host` / `docker` / `both`), `comment`.
- **Presets**: `ssh` (ports from `sshd -T`), `api` (the daemon API's port) and `web` (80/443 when the web server is installed) are derived at every apply, so they follow the host. Switching `ssh` or `api` off needs `--force`.
- **Sets**: `allowlist` and `blocklist` — nft sets with `flags interval,timeout`; an entry may carry an expiry.

### Rendered table

`input` chain (hook `input`, priority 0):

1. the blocklist, then connection tracking (`established,related` accepted, `invalid` dropped) and loopback;
2. the allowlist, ICMP errors and IPv6 neighbour discovery (always — dropping them breaks IPv6 and path MTU discovery), ping if allowed, DHCP replies;
3. deny rules (`drop` / `reject`), then allow rules — the order of the list does not matter;
4. the policy.

`forward` chain (priority -1) exists only when something needs it (a rule with scope `docker` or `both`, `protect_docker`, a non-empty blocklist). Every rule there matches `ct status dnat` and the *original* destination port, so outbound traffic of containers is never touched; the daemon never edits Docker's chains. `drop` is final, `accept` only exempts a connection from the daemon's own `protect_docker` drop.

Every rule carries `counter` and the comment `asc:<id>`; that is how packet counters map back to rules.

### Apply with automatic rollback

1. The rollback script is written to `/var/lib/asc/firewall/pending.json` **before** anything changes: the previous `inet asc` (or `delete table` when there was none); for RAW, the whole `nft list ruleset`.
2. The new ruleset is checked with `nft -c -f`, then loaded atomically (`table inet asc` + `delete table inet asc` + the new table in one `nft -f` transaction).
3. A timer starts (default 60 s, allowed 15–600 s; `--no-rollback` commits at once for scripts).
4. `confirm` commits: the script becomes `/etc/asc/firewall/ruleset.nft` and the unit is enabled, so **a reboot always returns to the last confirmed state**. When the deadline passes, the timer runs the rollback script.
5. A daemon restarted during the window finishes the job: an expired change is rolled back, a live one gets its timer back. It also reloads a confirmed table that the kernel lost.
6. Only one change can wait at a time.

### RAW mode

`ApplyFirewallRuleset(text, acknowledge_risk)` refuses without the acknowledgement, checks the text with `nft -c`, then runs `flush ruleset` and the text in one transaction under the same rollback. That replaces **every** table — Docker's and fail2ban's too; the rollback restores them from the dump. While RAW is active the managed model is stored but not applied. Leaving RAW (`disable`) keeps whatever the text loaded until the next boot.

### Persistence

`asc-firewall.service` (`Type=oneshot`, `After=nftables.service`, `Before=network-pre.target`, written by the daemon) loads `/etc/asc/firewall/ruleset.nft` at boot. The distribution's `/etc/nftables.conf` and `nftables.service` are not touched; ordering after the distribution's unit matters because its default config starts with `flush ruleset`.

### API (`FirewallService`)

| RPC | What it does |
|---|---|
| `GetFirewall` | Mode, settings, rules, presets, sets, pending change, conflicts, whether `nft` is installed, rule counters, unapplied changes |
| `InstallNftablesStream` | Installs the package, streaming the log |
| `UpdateFirewallSettings`, `UpsertFirewallRule`, `RemoveFirewallRule`, `SetFirewallIpSet` | Edit the model (stored, not applied) |
| `RenderFirewall` | The table the next apply would load, next to the one in force |
| `ApplyFirewall`, `ConfirmFirewall`, `RollbackFirewall`, `DisableFirewall` | Apply with the window, confirm, roll back now, switch off |
| `GetFirewallRuleset`, `ApplyFirewallRuleset` | RAW mode |
| `ListFirewallTables` | Every table of the host, read-only, with its owner (`asc`, `fail2ban`, `iptables`, `other`) |

Capabilities in `GetStatus`: `firewall`, `firewall.raw`. All calls are root-only. The same operations are available as REST under `/v1/firewall` for the CLI.

### CLI

`asc firewall status | install | enable | disable | allow | deny | rules | remove | set | settings | render | apply | confirm | rollback | ruleset | tables`. Command reference: <https://docs.adminservice.cloud/commands/firewall>.

## 🔗 Related

See also: [🚫 fail2ban](fail2ban.md) — it bans through its own nftables table next to `inet asc`; [🌐 webserver](webserver.md) — the installer only hints about the firewall and does not change it.
