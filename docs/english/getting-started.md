# 🚀 Getting Started

> 🌍 **Language:** English · [🇷🇺 Russian version](../russian/getting-started.md)

## 📌 Description

Install the `asc` daemon on a server, verify the system service and prepare it to run Docker applications. Debian and Ubuntu are first-class targets; x86_64, ARM64 and ARMv7 are supported.

## 🎯 Scenarios

### Interactive server installation

Log in to the server and run the installer with sudo. It installs `asc-updater`, which downloads the daemon and asks for the language and update settings. If Docker is missing, it offers to install it for container applications.

```bash
curl -fsSL https://raw.githubusercontent.com/AdminServiceCloud/asc-daemon/main/install.sh | sudo bash
```

### Unattended installation

Use the silent option for provisioning, CI, scripts and automation. It accepts the default settings and does not ask questions.

```bash
curl -fsSL https://raw.githubusercontent.com/AdminServiceCloud/asc-daemon/main/install.sh | sudo bash -s -- --silent
```

### Verify the result

```bash
asc status
asc service status
docker --version
```

`asc status` shows the installed version, service state and application summary. The installer creates and enables the `asc` systemd service; manage it with `sudo asc service start|stop|restart|status`.

## 🏗️ Technical design

- Installation requires root because it installs binaries, creates `/asc`, writes `/etc/asc/config.toml` and manages the systemd unit.
- `asc-updater` owns daemon installation, updates, channels and rollback, so it remains available if the daemon itself cannot start.
- Docker is required only for `type: docker` packages. Native and utility packages do not require it.
- Change the CLI language later with `sudo asc config lang en` or `sudo asc config lang ru`.

## 🔗 Related tasks

DMN-001, DMN-014, DMN-057 in [ROADMAP.md](../../../asc-platform/ROADMAP.md).
