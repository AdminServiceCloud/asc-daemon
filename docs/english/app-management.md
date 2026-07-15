# 📱 Application management (daemon)

## 📌 Description

The core of the daemon: a single interface for managing applications of three kinds — 🐳 Docker containers, 📦 native applications (systemd units) and 🔧 plain processes (PID). The key difference from Portainer/Coolify: not just Docker.

## 🎯 Scenarios

- `asc app install helloworld` — install from a registry (Docker or native — the manifest decides).
- `asc app start|stop|restart|status|logs <name>` — lifecycle management. `start` attaches to the app's console right away (Docker apps, interactive terminal — like `docker run` without `-d`); `asc app start -d <name>` starts detached, without attaching. When the host is short on the resources the manifest `requirements` declare, `start` warns with the figures and asks to continue at the user's own risk (DMN-029).
- **Custom names** (DMN-024): `asc install` asks for an app name (Enter keeps the default; non-interactively — `--name`). `asc app list` shows both the original ID and the NAME, and every command accepts either one: `asc app start "My Server"` = `asc app start cs2-server`. When an id and a name collide, the id wins; an ambiguous name (several apps named alike) is an error suggesting the id.
- **Multiple instances of one app** (DMN-033): installing an already-installed package no longer errors out — it becomes a new instance `<package>-2`, `<package>-3`, … (also its default name), so `asc install nginx` twice gives two independent `nginx`/`nginx-2` apps. Installing a whole stack again names its new instances the same way; `--name <prefix>` on a whole-stack install prefixes every member app instead (DMN-034), since a stack has no entity of its own.
- **`asc app disk <name>`** (DMN-035): disk usage — a quota bar when `quota.max_disk` is set, then a breakdown by Docker image, repository checkout, private data and custom volumes (Docker named volumes are marked shared and excluded from the app-directory total).
- `asc app list` — a user sees **only their own** applications; `sudo asc app list` — the applications of all users. Short aliases (DMN-025): `asc ls` and `asc ps` — same output and same permissions.
- `asc stats` — CPU and memory consumption per application (like `docker stats`, see [📊 monitoring](monitoring.md)).
- The platform performs the same operations through the daemon API.
- After a server reboot the daemon restores the application states (running/stopped).

## 🏗️ Technical design

### 👥 Application groups per user

- Each application belongs to the Linux user who installed it (the owner is recorded in `meta.json` and the index).
- A regular user sees and manages **only their own application group**.
- Via `sudo` (or as `root`) the applications of **all users** are visible and accessible — the output is grouped by owner.
- The daemon API applies the same rule: the request context determines the visible group.

### 📂 Application storage: /asc/apps/

Every application lives in a directory named after its ID:

```
/asc/apps/<id>/
├── config/        # ⚙️ application settings (see asc.settings.yaml in package-manager.md)
├── repository/    # 📦 the application's cloned repository (versions = git tags)
├── data/          # 💾 volumes — if the application runs in Docker
└── meta.json      # 📇 application info: id, name, custom name, owner, version (tag), source, state
```

- **Installation = cloning the repository** of the package into `repository/`; switching versions = checking out the desired git tag (details — [📦 package-manager](package-manager.md)).
- `meta.json` is the source of truth for rebuilding the index after a crash/reboot.

### ⚙️ Core

- **Drivers**: the `AppDriver { start, stop, restart, state, logs, remove }` trait with implementations `DockerDriver` (via the **Docker Engine API over the unix socket** — not the `docker` CLI; the socket path is configurable — `[docker] socket`, default `/var/run/docker.sock`), `SystemdDriver` (units `asc-app-<id>.service`), `ProcessDriver` (supervised PID: a pid file and a log file in the application directory). Application installation (creating a container/unit from the manifest) is the package manager's job (DMN-003).
- **Docker Engine API**: the daemon talks to Docker through the Engine API (the `bollard` client, unix socket), not the CLI — this works for rootless installations and non-standard socket locations (just set the path in the config). Control operations (start/stop/inspect/create/remove) are synchronous; log streaming and attach for the console are asynchronous over the same socket. Container creation pulls the image by itself when it is not on the host (a pull through the same Engine API); an Engine response with any HTTP status is not treated as Docker being unreachable — the user sees the Engine's own message.
- **Application index**: `meta.json` is the source of truth; in the MVP the index is built by scanning `/asc/apps/*/meta.json` on demand; at startup the daemon compares the desired state (`desired_state`) with reality (containers, units, processes) and restarts anything that has fallen over. A local database (SQLite) will appear once there is state beyond meta.json (metrics, operation history).
- **Logs**: a single interface — docker logs / journald / file; streaming out via [🖥️ console](console.md).
- **Cluster mode (post-MVP)**: multiple *nodes* running the platform together. Multiple instances of one application on a single node already work (DMN-033/034, above).
- **MVP CLI commands**: `asc status`, `asc stats`, `asc app list|install|remove|start|stop|restart|logs|info|disk|settings` (+ the `asc ls`/`asc ps` aliases for the list), `asc service` (managing the daemon itself).

## 🔗 Related tasks

DMN-002, DMN-004, FE-005 in [ROADMAP.md](../../../asc-platform/ROADMAP.md).
