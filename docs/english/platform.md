# 🔗 Connecting to the platform

> 🌍 **Language:** English · [🇷🇺 Русская версия](../russian/platform.md)

## 📌 Description

The daemon is fully autonomous: everything works through the CLI and the local
API without any platform account. Connecting to AdminService.Cloud is optional
and adds a management panel, an AI assistant, teams and roles on top.

The link is established with a **one-time registration token** issued in the
panel. The token can be passed straight to the installer, or supplied later
with `asc connect`.

## 🎯 Scenarios

- An operator copies the install command from the panel and runs it on a fresh
  server: the daemon installs itself and registers the node in one step.
- The platform installs the daemon itself over SSH and passes the same token
  along the way.
- A node installed earlier without a platform is attached to one later:
  `sudo asc connect <token>`.
- A registration attempt failed because the platform was unreachable; the
  daemon still works locally and the operator retries with `asc connect`.

## 🏗️ Technical design

### Install command

```bash
curl -fsSL https://raw.githubusercontent.com/AdminServiceCloud/asc-daemon/main/install.sh \
  | sudo bash -s -- --silent --token <TOKEN> --url https://adminservice.cloud
```

| Option | Meaning |
|---|---|
| `--silent` | Accept the defaults and ask nothing — the mode the platform uses. Docker is installed too: a node that cannot run container apps is not a working node |
| `--no-docker` | Never install Docker, not even with `--silent` |
| `--direct` | Expose the API to the network over TLS so the platform reaches it without an SSH tunnel |
| `--token <TOKEN>` | One-time registration token; binds the node to the organization that issued it |
| `--url <URL>` | Platform base URL, `https://adminservice.cloud` by default |

Both `--token <value>` and `--token=<value>` are accepted. The token is
validated before use and may contain only letters, digits, `-` and `_`.
`--url` alone is rejected: without a token there is nothing to register.

`install.sh` forwards the options to `asc-updater install`, which installs the
daemon first and registers afterwards.

### `asc connect`

```bash
sudo asc connect <TOKEN> [--url https://adminservice.cloud]
```

Requires root: the command writes root-owned files. It stores the token,
remembers the platform URL and calls the registration endpoint — the same path
the installer takes, without reinstalling anything. `asc status` shows the
platform and the node id, or reports that the node is not connected.

### Where the values are stored

| What | Where | Access |
|---|---|---|
| Registration token | `/etc/asc/platform.token` | `0600`, root only |
| Platform URL, node id, registration time | `[platform]` in `/etc/asc/config.toml` | `0644` |

The split follows the rule the config file already states: secrets never live
in `config.toml`, which is world-readable so that any user can read the
daemon's settings.

```toml
[platform]
url = "https://adminservice.cloud"
node_id = "0e2f…"
registered_at = "2026-08-20T09:31:04Z"
```

### Registration request

The daemon sends one request to the platform's public bootstrap endpoint:

```
POST <url>/bootstrap/asc.node.v1.BootstrapService/RegisterNode
Content-Type: application/json

{"token":"…","hostname":"…","primaryIp":"…","os":"…","arch":"x86_64","daemonVersion":"0.10.0"}
```

The body is sent through stdin rather than as a command argument, so the token
never appears in `ps` output. The platform redeems the token — it works exactly
once — and replies with the node and organization ids.

`http://` URLs are permitted for local development, with a warning: the token
crosses the network unencrypted.

### Failure handling

Registration never fails the installation. If the platform is unreachable, the
installer prints a warning and exits successfully: the daemon is installed and
usable locally, and `asc connect` retries whenever convenient.

### Liveness

The platform asks the daemon for status by calling
`asc.daemon.v1.DaemonService/GetStatus`. A node whose daemon answers is shown
as online; one that does not answer is offline. There are two ways the call can
reach the daemon.

**Through SSH (default).** The API stays on loopback and the platform forwards
to it over the SSH connection it already holds. Nothing is exposed to the
network; the platform reads `/etc/asc/api.token` during installation.

**Directly (`--direct`).** The API moves to `0.0.0.0` and serves TLS. The two
are switched on together and never apart: the bearer token grants full control
of the machine, so the port must not be open without encryption. The
certificate is self-signed — nodes have no domain of their own — and the
platform pins its SHA-256 fingerprint at registration, exactly as it pins an
SSH host key. The daemon hands its API token to the platform inside the
registration call, because without SSH there is no other way to deliver it.

A node installed with the plain command and no `--direct` cannot report its
status at all: the platform has neither SSH access nor an endpoint to dial.

```toml
[api]
listen = "0.0.0.0:8420"
tls = "self_signed"   # off | self_signed | acme | files
# A name the platform dials instead of the address, so the node survives an
# IP change. In self_signed mode it is baked into the certificate as a SAN.
# domain = "node.example.com"
# tls = "files" uses an operator-supplied certificate instead:
# tls_cert = "/etc/asc/fullchain.pem"
# tls_key  = "/etc/asc/privkey.pem"
```

The certificate and key live next to config.toml as `api.crt` (0644) and
`api.key` (0600). `asc api tls` and `asc api listen` write these settings and
validate them before they land, so a configuration that cannot work is refused
where it is entered rather than at the next start.

### 🔐 How the certificate is trusted (DMN-067)

| `tls` | What vouches for the certificate |
|---|---|
| `self_signed` | Nothing does, so the platform pins its SHA-256 fingerprint at registration — the same contract as an SSH host key. |
| `acme` | A public CA would, once the daemon can obtain a certificate itself. **Not available yet:** the daemon refuses to start in this mode; use `files` with a certbot-managed certificate for the same result. |
| `files` | Whoever issued the certificate you installed. The platform verifies the chain and the host name; when the daemon also reports a fingerprint — a certificate from a private CA — it pins that instead. |

A fingerprint is reported **only** for `self_signed`. Pinning a chain-verified
certificate would break the node the first time it renews.

### 📡 Reporting a change afterwards (DMN-068)

Registration is a one-shot token redemption, so it cannot carry the news that
an address or certificate changed later. The daemon calls
`BootstrapService.ReportNodeEndpoint` instead — at startup when the computed
endpoint differs from the last one reported, and after `asc api tls`. The call
authenticates with the node's primary API token, the one secret both sides
already share, and it can change only how the node is reached: never its
organization, and never a deleted node.

What was last reported is cached in config.toml (`[platform] reported_endpoint`
/ `reported_fingerprint`) so a daemon that restarts with nothing changed does
not call the platform on every boot.

### What is not implemented yet

There is no persistent channel to the platform. A registered node reports its
facts once and stops there — it does not send heartbeats and the panel does not
show it as online. Commands from the panel, log streaming and metrics arrive
with the platform tunnel (NODE-002 on the platform side).

## 🔗 Related tasks

DMN-058 in the [ROADMAP](../../../asc-platform/ROADMAP.md); on the platform
side — NODE-001 (node registry and registration tokens) and NODE-002 (the
platform ↔ daemon channel).
