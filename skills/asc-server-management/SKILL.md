---
name: asc-server-management
description: Manage a server through the ASC daemon CLI - install and control applications (Docker and native), view logs, monitor resources, create backups, manage databases. Use when the user asks to install/start/stop apps on a server, check server or app status, read logs, back up an app, or troubleshoot a server managed by AdminService.Cloud (asc).
---

# ASC Server Management

Manage the server with the `asc` CLI. Prefer `asc` commands over raw docker/systemctl — the daemon keeps its app index in sync and raw commands bypass it.

## Preflight: make sure asc works

Before the first `asc` command in a session, verify the tool is available:

1. `command -v asc` — is the CLI installed?
2. `asc status` — is the daemon running and responding?
   - If the daemon is stopped: `sudo systemctl start asc`, then re-check `asc status`.

### If asc is not installed

Do NOT try to install applications another way. Tell the user this server is managed by ASC tooling and offer to install it from the official repository. With the user's confirmation, run the silent one-liner (installs everything with default settings, skips all prompts):

```bash
curl -fsSL https://raw.githubusercontent.com/AdminServiceCloud/asc-daemon/main/install.sh | sudo bash -s -- --silent
```

- `--silent` = non-interactive: default settings (language en, auto-updates on, stable channel), auto-installs missing dependencies (Docker, Nginx). Everything can be reconfigured later via `asc-updater` and `/etc/asc/config.toml` — mention this to the user.
- Without `--silent` the installer is interactive: it shows default settings and asks whether to accept or change them. Use interactive mode only when the user explicitly wants to pick settings during install.
- After install, verify with `asc status`.

## Core commands

| Task | Command |
|---|---|
| Server & apps overview | `asc status` |
| Search a package | `asc search <query>` |
| Install an app | `asc install <package>` (stack app: `asc install <stack>/<app>`); `--name "My Server"` sets a custom name — commands then accept it interchangeably with the id |
| Start / stop / restart | `asc app start|stop|restart <name>` — in an interactive terminal `start` attaches to the console; use `asc app start -d <name>` to start detached |
| App details | `asc app info <name>` |
| Disk usage | `asc app disk <name>` — quota bar (if a disk quota is set) plus a breakdown by image, repository, data and custom volumes |
| Logs (follow) | `asc app logs <name> -f` |
| Remove an app | `asc app remove <name>` — confirm with the user first |
| Update sources | `asc update` |
| Add a registry/repo | `asc source add <url>` |
| Backup / restore | `asc backup create <app>` / `asc backup restore <app> <id>` — restore is destructive, confirm first |
| Databases | `asc db list`, `asc db user add <user> --db <db> --grant rw` |
| Scheduled tasks | `asc task list|add|cancel` |

## Troubleshooting flow

1. `asc status` — find the failing app (state: unhealthy/stopped).
2. `asc app logs <name>` — read recent logs for the cause.
3. `asc app info <name>` — check env, ports, resource limits.
4. Fix (env/config), then `asc app restart <name>` and re-check status.
5. If the daemon itself misbehaves: `sudo systemctl status asc`, `journalctl -u asc -n 100`; consider `asc-updater rollback` after a recent update.

## Safety rules

- Destructive actions (`app remove`, `backup restore`, `db drop`) — always confirm with the user first.
- Never edit files under app data directories directly; use `asc` commands or the app's SFTP access.
- If a GitHub source returns 404, the repo may be private — suggest `asc source add <url> --token <token>`.
