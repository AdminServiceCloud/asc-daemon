# 🦀 asc-daemon — AdminService.Cloud Daemon

> 🌍 **Language:** English · [🇷🇺 Русская версия](docs/russian/README.md)

**Open source utility for managing runtime applications on Linux**: a daemon and CLI written in Rust that launch and maintain applications of any runtime — Docker containers, native applications (systemd) and plain processes. Included: a package manager, backups, monitoring, and an MCP server for AI. Works standalone via the CLI or as an agent of the [AdminService.Cloud](https://adminservice.cloud) SaaS platform.

## ✨ Features

- 📱 **Application management** — Docker containers, native applications (systemd) and system utilities
- 💻 **CLI** — full server management from the terminal (`asc ...`)
- 📡 **ConnectRPC + REST API** — the API proto contracts live in this repository (`proto/`) and ship together with the daemon; the AdminService.Cloud platform links them from here. REST (JSON/HTTP) runs alongside ConnectRPC on the same server — from the same contracts
- 📦 **Package manager** — an `asc.yaml` manifest, registries (like apt) and installation via `asc install <package>`
- 🤖 **MCP server** — manage the server through AI (Claude Code, Claude Desktop, the ASC platform)
- 💾 **Backups** — full and incremental, local and to the cloud (S3/SFTP), with rotation
- 📊 **Monitoring** — system and application metrics, health checks
- 📁 **SFTP server** — file access isolated to a specific application
- 🗄️ **Databases** — create databases and users (PostgreSQL, MySQL, MongoDB, Redis)
- 🖥️ **Consoles** — WebSocket application terminal and SSH console for the UI
- ⏰ **Scheduler** — scheduled (cron) tasks, a priority queue
- 🔄 **asc-updater** — a separate updater utility: auto-updates (can be disabled), stable/beta channels, rollback; at install time — pick default settings or your own
- 🧠 **Skills for AI agents** — ready-made skills for Claude Code and other models in the [skills/](skills/README.md) directory

## ⚡ Installation

```bash
# interactive: shows the default settings and asks — accept or change
curl -fsSL https://raw.githubusercontent.com/AdminServiceCloud/asc-daemon/main/install.sh | sudo bash

# silent mode: one command, everything installed with defaults and no questions
curl -fsSL https://raw.githubusercontent.com/AdminServiceCloud/asc-daemon/main/install.sh | sudo bash -s -- --silent
```

You can also configure it after a silent install: `asc-updater` + `/etc/asc/config.toml`.

## ⌨️ Quick start

```bash
asc service install        # ⚙️ install the daemon's API service as a systemd unit (autostart)
asc service start|status   # 🚀 start the service / check its state
asc status                 # 📊 server and application status
asc install helloworld     # 📦 install an application from the registry
asc app logs helloworld    # 📜 application logs
asc config lang ru         # 🌍 change the CLI output language (en|ru)
asc connect <token>        # ☁️ connect to the AdminService.Cloud platform
asc mcp serve              # 🤖 run the MCP server for AI clients
```

## 🧠 Skills for Claude Code and other models

The [skills/](skills/README.md) directory contains ready-made Agent Skills that teach AI agents to manage the server through `asc`:

```bash
# Claude Code: install the skills for yourself (all projects) or in a project
cp -r skills/* ~/.claude/skills/       # globally
cp -r skills/* .claude/skills/         # current project only
```

| Skill | What it does |
|---|---|
| [🖥️ asc-server-management](skills/asc-server-management/SKILL.md) | Server management: applications, logs, backups, databases. If `asc` is not installed, it checks, then offers to install it from the official repository with a single command (silent mode) |
| [📦 asc-app-packaging](skills/asc-app-packaging/SKILL.md) | Packaging applications: `asc.yaml` / `asc.stack.yaml`, validation against schemas, publishing to a registry |

For MCP clients (Claude Desktop and others) use the [daemon's MCP server](docs/mcp-server.md) instead of skills: `asc mcp serve`.

## 📋 Requirements

- 🐧 **OS**: **Debian and Ubuntu** are supported today; other distributions (CentOS/RHEL, Fedora, Arch, etc.) and macOS are planned
- 🧬 **Architectures**: x86_64, ARM64, ARMv7
- 🔑 Root/sudo for installation; Docker is installed automatically when needed
- ⚙️ systemd (for `asc service` and autostart)

## 📚 Documentation

Documentation for the daemon's modules lives in the [docs/](docs/README.md) directory:

| Doc | Description |
|---|---|
| [🦀 Daemon overview](docs/README.md) | Architecture, API, installation |
| [📡 api](docs/api.md) | gRPC (ConnectRPC) + REST on one port, tokens |
| [📱 app-management](docs/app-management.md) | Docker and native applications, CLI |
| [📦 package-manager](docs/package-manager.md) | asc.yaml, registries, `asc install` |
| [🤖 mcp-server](docs/mcp-server.md) | MCP server for AI |
| [📊 monitoring](docs/monitoring.md) | System and application metrics |
| [💾 backups](docs/backups.md) | Application backups |
| [📁 sftp](docs/sftp.md) | SFTP isolated per application |
| [🗄️ database](docs/database.md) | Database management |
| [🖥️ console](docs/console.md) | WebSocket and SSH consoles |
| [⏰ scheduler](docs/scheduler.md) | Task scheduler |
| [🔄 updater](docs/updater.md) | The asc-updater utility: auto-updates, channels, rollback |

> 📝 Module docs are being translated to English (task CORE-011); some pages may still be in Russian for now.

## 🗺️ Roadmap and process

The roadmap for the whole project is kept in the **asc-platform** repository:

- [🎯 ROADMAP](../asc-platform/ROADMAP.md) — daemon tasks use the `DMN-*` prefix
- [🤝 Development process](../asc-platform/AGENTS.md)

## 🤝 Contributing

Contribution rules are in [CONTRIBUTING.md](CONTRIBUTING.md); CI and releases run on GitHub Actions (`.github/workflows/`).

## 📄 License

[MIT](LICENSE) — free to distribute, modify and use commercially, but with **mandatory attribution**: Omar El Sayed ([@statebyte](https://github.com/statebyte)), the AdminService.Cloud project, [Anytecture Software](https://anytecture.com).
