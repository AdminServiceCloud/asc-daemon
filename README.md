# 🦀 ASC Daemon CLI

> 🌍 **Language:** English · [🌐 Other Languages](docs/README.md)



[![CI](https://github.com/AdminServiceCloud/asc-daemon/actions/workflows/ci.yml/badge.svg)](https://github.com/AdminServiceCloud/asc-daemon/actions/workflows/ci.yml)
[![Release](https://github.com/AdminServiceCloud/asc-daemon/actions/workflows/release.yml/badge.svg)](https://github.com/AdminServiceCloud/asc-daemon/actions/workflows/release.yml)
[![Version](https://img.shields.io/badge/version-0.3.4-blue)](version.txt)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Discord](https://img.shields.io/badge/Discord-Join%20us-5865F2?logo=discord&logoColor=white)](https://discord.gg/xzJfp3ePfV)

![asc-daemon preview](docs/screenshots/preview.png)

## 📌 About

**Open source utility for managing runtime applications on Linux**: a daemon and CLI written in Rust that launch and maintain applications of any runtime — Docker containers, native applications (systemd) and plain processes. Included: a package manager, backups, monitoring, and an MCP server for AI. Works standalone via the CLI or as an agent of the [AdminService.Cloud](https://adminservice.cloud) SaaS platform. Runs in any Linux environment, including **WSL** (Windows Subsystem for Linux).

### ✨ Key features

- 📱 **Application management** — Docker containers, native applications (systemd) and system utilities
- 💻 **CLI** — full server management from the terminal (`asc ...`)
- 🧬 **Instance cloning** — `asc app clone <id>`: a full copy of an application instance (data, env, settings) with a new id; via the AdminService.Cloud UI a clone can be moved to another node
- 📡 **ConnectRPC + REST API** — the API proto contracts live in this repository (`proto/`) and ship together with the daemon; the AdminService.Cloud platform links them from here. REST (JSON/HTTP) runs alongside ConnectRPC on the same server — from the same contracts
- 📦 **Package manager** — an `asc.yaml` manifest, registries (like apt) and installation via `asc install <package>`
- 🤖 **MCP server** — manage the server through AI (Claude Code, Claude Desktop, the ASC platform)
- 💾 **Backups** — `asc backup create|restore|list|prune`, a local storage out of the box, S3/FTP/SFTP storages configurable (upload not wired up yet), exclusions via `asc.backup.yaml`, rotation
- 📊 **Monitoring** — system and application metrics, health checks
- 📁 **SFTP server** — file access isolated to a specific application
- 🖥️ **Consoles** — WebSocket application terminal and SSH console for the UI
- ⏰ **Scheduler** — scheduled (cron) tasks, a priority queue
- 🔄 **asc-updater** — a separate updater utility: auto-updates (can be disabled), stable/beta channels, rollback; at install time — pick default settings or your own
- 🧠 **Skills for AI agents** — ready-made skills for Claude Code and other models in the [skills/](skills/README.md) directory

### 💡 Motivation

Existing panels solve only part of the problem: Portainer manages Docker, Pterodactyl manages game servers, classic hosting panels manage websites. As soon as a real server mixes containers with native services, you are back to raw SSH. **asc-daemon** was born to manage *any* runtime through one tool — Docker containers, systemd services and plain processes — with the same commands, the same API and the same package manager. And because AI agents are becoming the way servers get managed, the daemon speaks MCP natively, so an AI can operate your server as a first-class client. The daemon is fully autonomous: everything works locally via the CLI, no platform account required.

## ⚡ Installation

```bash
curl -fsSL https://raw.githubusercontent.com/AdminServiceCloud/asc-daemon/main/install.sh | sudo bash
```
> interactive: shows the default settings and asks — accept or change

```bash
curl -fsSL https://raw.githubusercontent.com/AdminServiceCloud/asc-daemon/main/install.sh | sudo bash -s -- --silent
```
> silent mode: one command, everything installed with defaults and no questions

You can also configure it after a silent install: `asc-updater` + `/etc/asc/config.toml`.

### 📋 Requirements

- 🐧 **OS**: **Debian and Ubuntu** are supported today, including under **WSL** (Windows Subsystem for Linux); other distributions (CentOS/RHEL, Fedora, Arch, etc.) and macOS are planned
- 🧬 **Architectures**: x86_64, ARM64, ARMv7
- 🔑 Root/sudo for installation; Docker is installed automatically when needed
- ⚙️ systemd (for `asc service` and autostart)

## ⌨️ Quick start

```bash
asc service install
```
> ⚙️ install the daemon's API service as a systemd unit (autostart)

```bash
asc service start|status
```
> 🚀 start the service / check its state

```bash
asc status
```
> 📊 server and application status

```bash
asc install helloworld
```
> 📦 install an application from the registry

```bash
asc ls | asc ls ports | asc ls disk | asc ls stats
```
> 📋 list apps, or switch the same list to published ports, disk usage or live stats (`asc ports` / `asc disk` / `asc stats` are the standalone equivalents)

```bash
asc stacks
```
> 🗂️ installed stacks and their member apps, hierarchically

```bash
asc app logs helloworld
```
> 📜 application logs

```bash
asc app clone helloworld
```
> 🧬 clone an application instance (data, env, settings)

```bash
asc app settings helloworld
```
> 🎛️ interactive settings editor (types, limits and enums from asc.settings.yaml)

```bash
asc backup create helloworld
```
> 💾 back up an app (local storage by default; `asc backup restore <app> <backup-id>` to restore)

```bash
asc config lang ru
```
> 🌍 change the CLI output language (en|ru)

```bash
asc connect <token>
```
> ☁️ connect to the AdminService.Cloud platform

```bash
asc mcp serve
```
> 🤖 run the MCP server for AI clients

## 🧠 Skills for Claude Code and other models

The [skills/](skills/README.md) directory contains ready-made Agent Skills that teach AI agents to manage the server through `asc`:

```bash
cp -r skills/* ~/.claude/skills/
```
> Claude Code: install the skills globally (for all projects)

```bash
cp -r skills/* .claude/skills/
```
> Claude Code: install the skills for the current project only

| Skill | What it does |
|---|---|
| [🖥️ asc-server-management](skills/asc-server-management/SKILL.md) | Server management: applications, logs, backups. If `asc` is not installed, it checks, then offers to install it from the official repository with a single command (silent mode) |
| [📦 asc-app-packaging](skills/asc-app-packaging/SKILL.md) | Packaging applications: `asc.yaml` / `asc.stack.yaml`, validation against schemas, publishing to a registry |

For MCP clients (Claude Desktop and others) use the [daemon's MCP server](docs/english/mcp-server.md) instead of skills: `asc mcp serve`.

## 📚 Documentation

Documentation for the daemon's modules lives in the [docs/english/](docs/english/README.md) directory:

| Doc | Description |
|---|---|
| [🦀 Daemon overview](docs/english/README.md) | Architecture, API, installation |
| [📡 api](docs/english/api.md) | gRPC (ConnectRPC) + REST on one port, tokens |
| [📱 app-management](docs/english/app-management.md) | Docker and native applications, CLI |
| [📦 package-manager](docs/english/package-manager.md) | asc.yaml, registries, `asc install` |
| [🤖 mcp-server](docs/english/mcp-server.md) | MCP server for AI |
| [📊 monitoring](docs/english/monitoring.md) | System and application metrics |
| [💾 backups](docs/english/backups.md) | Application backups |
| [📁 sftp](docs/english/sftp.md) | SFTP isolated per application |
| [🖥️ console](docs/english/console.md) | WebSocket and SSH consoles |
| [⏰ scheduler](docs/english/scheduler.md) | Task scheduler |
| [🔄 updater](docs/english/updater.md) | The asc-updater utility: auto-updates, channels, rollback |

## 🗺️ Roadmap

The roadmap for the whole project is kept in the **asc-platform** repository:

- [🎯 ROADMAP](../asc-platform/ROADMAP.md) — daemon tasks use the `DMN-*` prefix
- [🤝 Development process](../asc-platform/AGENTS.md)

## 💬 Support

Reach out to the maintainers through any of these channels:

- 🐛 [GitHub Issues](https://github.com/AdminServiceCloud/asc-daemon/issues) — bug reports and feature requests (templates included)
- ❓ [GitHub Discussions](https://github.com/AdminServiceCloud/asc-daemon/discussions) — questions and ideas
- 💬 [Discord](https://discord.gg/xzJfp3ePfV) — the official community server: chat, help, announcements
- ☁️ [adminservice.cloud](https://adminservice.cloud) — the platform website and contact options

## 🌟 Project assistance

If you want to say **thank you** or support the active development of asc-daemon:

- ⭐ Add a GitHub star to the repository
- 🐦 Share the project on social media
- 📝 Write about the project on your blog or at meetups
- 💬 Join the [Discord community](https://discord.gg/xzJfp3ePfV)
- 🤝 [Contribute](CONTRIBUTING.md) — code, docs, translations, registry packages

## 🌠 Star History

<a href="https://www.star-history.com/?repos=AdminServiceCloud%2Fasc-daemon&type=date&legend=top-left">
 <picture>
   <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/chart?repos=AdminServiceCloud/asc-daemon&type=date&theme=dark&legend=top-left&sealed_token=v0FJVSk7VB6NmVGA5YhYX-nYfTTzS2tsXBuPBLTkUi07Hftpgyxkyw1i2hEakvPUy_ke6MAPn_pTa1-aRU3MUOONNeo8dV-72nvOtDhvtY8-wzYEqzFMsvTYR-easTVZLwn5_i0TYXyg0dl3q4cyYLI3nwb2YLoLzl8pQ8ACudwsd_11uP9LdZkTYzk-" />
   <source media="(prefers-color-scheme: light)" srcset="https://api.star-history.com/chart?repos=AdminServiceCloud/asc-daemon&type=date&legend=top-left&sealed_token=v0FJVSk7VB6NmVGA5YhYX-nYfTTzS2tsXBuPBLTkUi07Hftpgyxkyw1i2hEakvPUy_ke6MAPn_pTa1-aRU3MUOONNeo8dV-72nvOtDhvtY8-wzYEqzFMsvTYR-easTVZLwn5_i0TYXyg0dl3q4cyYLI3nwb2YLoLzl8pQ8ACudwsd_11uP9LdZkTYzk-" />
   <img alt="Star History Chart" src="https://api.star-history.com/chart?repos=AdminServiceCloud/asc-daemon&type=date&legend=top-left&sealed_token=v0FJVSk7VB6NmVGA5YhYX-nYfTTzS2tsXBuPBLTkUi07Hftpgyxkyw1i2hEakvPUy_ke6MAPn_pTa1-aRU3MUOONNeo8dV-72nvOtDhvtY8-wzYEqzFMsvTYR-easTVZLwn5_i0TYXyg0dl3q4cyYLI3nwb2YLoLzl8pQ8ACudwsd_11uP9LdZkTYzk-" />
 </picture>
</a>

## 🤝 Contributing

Contribution rules are in [CONTRIBUTING.md](CONTRIBUTING.md); CI and releases run on GitHub Actions (`.github/workflows/`). Developing on Windows? Use **WSL** (Ubuntu) to build and test the project: `cargo build` / `cargo test` run in WSL, while `cargo check` / `clippy` work from the host against the Linux target (see `.cargo/config.toml`). Every pull request automatically gets a review from the code owner ([@statebyte](https://github.com/statebyte)) via [CODEOWNERS](.github/CODEOWNERS).

Please read our [🤝 Code of Conduct](CODE_OF_CONDUCT.md) before participating — we are committed to a welcoming and harassment-free community.

## 👥 Authors & contributors

The original setup of this repository is by **Omar El Sayed** ([@statebyte](https://github.com/statebyte)), AdminService.Cloud, [Anytecture Software](https://anytecture.com).

For a full list of all authors and contributors, see the [contributors page](https://github.com/AdminServiceCloud/asc-daemon/graphs/contributors).

## 🛡️ Security

asc-daemon follows good practices of security, but 100% security cannot be assured. The software is provided **"as is"** without any warranty.

Found a vulnerability? Please report it privately — see our [🛡️ Security Policy](SECURITY.md).

## 📄 License

[MIT](LICENSE) — free to distribute, modify and use commercially, but with **mandatory attribution**: Omar El Sayed ([@statebyte](https://github.com/statebyte)), the AdminService.Cloud project, [Anytecture Software](https://anytecture.com).
