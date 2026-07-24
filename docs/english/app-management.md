# 📱 Application management (daemon)

## 📌 Description

The core of the daemon: a single interface for managing applications of three kinds — 🐳 Docker containers, 📦 native applications (systemd units) and 🔧 plain processes (PID). The key difference from Portainer/Coolify: not just Docker.

## 🎯 Scenarios

- `asc app install helloworld` — install from a registry (Docker or native — the manifest decides).
- `asc app start|stop|restart|status|logs <name>` — lifecycle management. `start` attaches to the app's console right away (Docker apps, interactive terminal — like `docker run` without `-d`); `asc app start -d <name>` starts detached, without attaching. When the host is short on the resources the manifest `requirements` declare, `start` warns with the figures and asks to continue at the user's own risk (DMN-029).
- **Custom names** (DMN-024): `asc install` asks for an app name (Enter keeps the default; non-interactively — `--name`). `asc app list` shows both the original ID and the NAME, and every command accepts either one: `asc app start "My Server"` = `asc app start cs2-server`. When an id and a name collide, the id wins; an ambiguous name (several apps named alike) is an error suggesting the id.
- **Multiple instances of one app** (DMN-033): installing an already-installed package no longer errors out — it becomes a new instance `<package>-2`, `<package>-3`, … (also its default name), so `asc install nginx` twice gives two independent `nginx`/`nginx-2` apps. Installing a whole stack again names its new instances the same way; `--name <prefix>` on a whole-stack install prefixes every member app instead (DMN-034), since a stack has no entity of its own.
- **`asc app disk <name>`** (DMN-035): disk usage — a quota bar when `quota.max_disk` is set, then a breakdown by Docker image, repository checkout, private data and custom volumes (Docker named volumes are marked shared and excluded from the app-directory total).
- **`asc app clone <name> [--name <clone-name>]`** (DMN-019): a full copy of an app instance — repository, config and data — under a new id (the same `<id>-N` numbering a repeat `asc install` uses, DMN-033), with a live byte-progress bar over the copy on a terminal. The runtime itself is never copied (a Docker container/systemd unit/process cannot be), only recreated from the copied manifest and settings, exactly like a fresh install. The clone always starts stopped. Cloning across nodes (moving the copy to a different server via the platform) is a separate, later increment — see [🧬 app-cloning](../../../asc-platform/docs/features/app-cloning.md).
- `asc app list` — a user sees **only their own** applications; `sudo asc app list` — the applications of all users. Short aliases (DMN-025): `asc ls` and `asc ps` — same output and same permissions.
- **`asc ports [<name>]`** / **`asc app ports [<name>]`** (DMN-049): the published host==container ports of an app with their transport (`27015/tcp`, `27015/udp`, or `27015/tcp+udp` when both share the port), resolved from the app's `type: ports` settings — so a **stopped** app still reports the ports it will bind on the next start. Without a name: a table of every visible app and its ports (root sees all users' apps).
- `asc stats [--live] [--sort cpu|mem]` — CPU, memory and disk consumption per application (like `docker stats`, see [📊 monitoring](monitoring.md)).
- **List subcommands** (DMN-049): `asc ls ports`, `asc ls disk` and `asc ls stats` switch the same app list to the ports, disk-usage or live-stats view — mirrors of `asc ports` / `asc disk` / `asc stats`.
- **`asc stacks`** (DMN-051): installed apps grouped by the stack (`asc.stack.yaml` package) they came from, rendered as a tree — the stack name annotated with how many of its apps are running (`[running/total]`), then its member apps as `├──`/`└──` branches (id, name, kind, state, version, uuid — same columns as `asc ls`), root sees all users' apps. The stack is read from `meta.package` (`"<stack>/<app>"`, recorded at install — see [📦 package-manager](package-manager.md)); an app installed on its own has no `/` in `package` and never appears here.
- The platform performs the same operations through the daemon API.
- After a server reboot the daemon restores the application states (running/stopped).

## 🏗️ Technical design

### 👥 Application groups per user

- Each application belongs to the Linux user who installed it (the owner is recorded in `meta.json` and the index).
- A regular user sees and manages **only their own application group**.
- Via `sudo` (or as `root`) the applications of **all users** are visible and accessible — the output is grouped by owner.
- The daemon API applies the same rule: the request context determines the visible group.
- **With the system daemon running** (DMN-042), the lifecycle commands (`ls`/`status`/`install`/`app start|stop|restart|logs|remove|info`) go through its unix socket `/run/asc/asc.sock`: the daemon reads the caller's uid from the kernel (SO_PEERCRED) and enforces this rule on the shared system store — a regular user manages their apps in `/asc/apps` **without sudo and without the docker group**, and `asc ls` / `sudo asc ls` finally agree on what is installed. Details — [📡 api](api.md).

### 📂 Application storage: /asc/apps/

Every application lives in a directory named after its ID:

```
/asc/apps/<id>/
├── config/        # ⚙️ application settings (see asc.settings.yaml in package-manager.md)
├── repository/    # 📦 the application's cloned repository (versions = git tags)
├── data/          # 💾 volumes — if the application runs in Docker
└── meta.json      # 📇 application info: id, uuid, name, custom name, owner, version (tag), source, state
```

- **Installation = cloning the repository** of the package into `repository/`; switching versions = checking out the desired git tag (details — [📦 package-manager](package-manager.md)).
- `meta.json` is the source of truth for rebuilding the index after a crash/reboot.
- **Path scoping by user**: `/asc/apps/` (with `/etc/asc/config.toml` and `/var/lib/asc`) is the tree of the **root** installation — the system daemon and `sudo asc`. Running `asc` as a regular user **without a running system daemon** works against a private tree under `~/.asc/` instead: `~/.asc/apps`, `~/.asc/data`, `~/.asc/config.toml` — so the user edits their apps' settings and config without sudo. With the daemon present, the lifecycle commands operate on the shared system tree through the daemon socket instead (DMN-042, see above). The root-managed `[policy]` section is still read from the system config and cannot be overridden per user.

#### 🆔 Instance UUID (DMN-044)

Alongside the `id`, every instance gets a **UUID** generated at `asc install` and stored in `meta.json` as `uuid`. It survives upgrades and is shown as the last column of `asc ls`:

```
ID          NAME        KIND     STATE       VERSION  UUID
pingpong    Ping Pong   docker   stopped     0.1.0    6f8a1c2e-3b4d-4e5f-8a9b-0c1d2e3f4a5b
legacy-app  Legacy App  docker   stopped     1.2.0    -
```

Why a second identifier: an `id` is **reusable**. Removing `helloworld-2` frees that id for the next install (DMN-033), so anything that outlives the app — stored credentials (DMN-045), platform records, audit history — would silently re-bind to a different application. A UUID is retired with the instance and never reissued.

- Generated as a random UUIDv4 (RFC 4122) from `/dev/urandom` — no extra dependency for sixteen bytes.
- **Optional in `meta.json`**: apps installed before DMN-044 have no `uuid` key, load normally and display `-`. Nothing is backfilled behind the user's back.
- `asc app clone` gives the clone its **own** UUID — it is a separate instance, not a copy of an identity.
- Exposed by the API as `App.uuid` (proto field 10, REST `uuid`), absent when unset.

### ⚙️ Core

- **Drivers**: the `AppDriver { start, stop, restart, state, logs, remove }` trait with implementations `DockerDriver` (via the **Docker Engine API over the unix socket** — not the `docker` CLI; the socket path is configurable — `[docker] socket`, default `/var/run/docker.sock`), `SystemdDriver` (units `asc-app-<id>.service`), `ProcessDriver` (supervised PID: a pid file and a log file in the application directory). Application installation (creating a container/unit from the manifest) is the package manager's job (DMN-003).
- **Docker Engine API**: the daemon talks to Docker through the Engine API (the `bollard` client, unix socket), not the CLI — this works for rootless installations and non-standard socket locations (just set the path in the config). Control operations (start/stop/inspect/create/remove) are synchronous; log streaming and attach for the console are asynchronous over the same socket. Container creation pulls the image by itself when it is not on the host (a pull through the same Engine API); an Engine response with any HTTP status is not treated as Docker being unreachable — the user sees the Engine's own message.
- **Application index**: `meta.json` is the source of truth; in the MVP the index is built by scanning `/asc/apps/*/meta.json` on demand; at startup the daemon compares the desired state (`desired_state`) with reality (containers, units, processes) and restarts anything that has fallen over. A local database (SQLite) will appear once there is state beyond meta.json (metrics, operation history).
- **Logs**: a single interface — docker logs / journald / file; streaming out via [🖥️ console](console.md).
- **Cluster mode (post-MVP)**: multiple *nodes* running the platform together. Multiple instances of one application on a single node already work (DMN-033/034, above).
- **MVP CLI commands**: `asc status`, `asc stats`, `asc ports`, `asc stacks`, `asc app list|install|remove|start|stop|restart|logs|info|disk|ports|clone|settings` (+ the `asc ls`/`asc ps` aliases for the list, and `asc ls ports|disk|stats` for the ports/disk/stats views), `asc service` (managing the daemon itself).

## 🔗 Related tasks

DMN-002, DMN-004, DMN-019, DMN-044, DMN-051, FE-005 in [ROADMAP.md](../../../asc-platform/ROADMAP.md).
