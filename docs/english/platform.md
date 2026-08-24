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
| `--silent` | Accept the defaults and ask nothing — the mode the platform uses |
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

### What is not implemented yet

There is no persistent channel to the platform. A registered node reports its
facts once and stops there — it does not send heartbeats and the panel does not
show it as online. Commands from the panel, log streaming and metrics arrive
with the platform tunnel (NODE-002 on the platform side).

## 🔗 Related tasks

DMN-058 in the [ROADMAP](../../../asc-platform/ROADMAP.md); on the platform
side — NODE-001 (node registry and registration tokens) and NODE-002 (the
platform ↔ daemon channel).
