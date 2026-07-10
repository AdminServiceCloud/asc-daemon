# 📦 Package manager and registries

## 📌 Description

A package manager in the spirit of apt/homebrew: applications are described by an `asc.yaml` manifest, published in registries and installed with `asc install <package>`. Registries can be official or custom, local (`file://`) or remote (`https://`), including GitHub repositories.

## 🎯 Scenarios

- `asc install nginx` — install from the official registry.
- `asc source add https://registry.example.com` — connect a custom registry (like an apt source).
- `asc source add https://github.com/user/my-app` — an application straight from GitHub (asc.yaml at the root); for a private repo a 404 triggers an offer to configure a token.
- The platform store ([🛍️ app-store](../../../asc-platform/docs/features/app-store.md)) is a storefront over the same registries.

## 🏗️ Technical design

### The asc.yaml manifest (draft)

```yaml
name: my-app
version: 1.2.0
type: docker | native | utility
category: web                 # a registry topic (databases, ai, bots, game-servers…)
description: "..."            # EN in the official registry
settings: ./asc.settings.yaml # optional: the application settings file (see below)
runtime:
  image: nginx:1.27           # for docker
  # or install/start/stop commands for native
env:
  - name: PORT
    default: 8080
ports: [8080]
volumes: [/data]
requirements: { ram: 256M, disk: 1G }
healthcheck: { http: /health }
```

> 📐 JSON schemas of the manifests: [asc.schema.json](../../../registry/schema/asc.schema.json), [asc.stack.schema.json](../../../registry/schema/asc.stack.schema.json) and [asc.settings.schema.json](../../../registry/schema/asc.settings.schema.json) in the `registry` repository.

### Application settings: asc.settings.yaml

Application settings live in a separate file referenced from `asc.yaml` (`settings: ./asc.settings.yaml`). It describes the parameters: type, limits, enumerations, defaults — the user fills them in at install time (the platform UI renders a form, the CLI asks questions; silent mode takes the defaults):

```yaml
settings:
  - key: server_name
    type: string                # string | number | boolean | enum | secret
    title: "Server name"
    default: "My Server"
    required: true

  - key: max_players
    type: number
    default: 10
    limits: { min: 1, max: 200 }   # value limits

  - key: difficulty
    type: enum                  # an enumeration
    values: [peaceful, easy, normal, hard]
    default: normal

  - key: rcon_password
    type: secret                # stored as a secret, masked
    required: true

  - key: enable_backups
    type: boolean
    default: true
```

- Setting values are saved in `/asc/apps/<id>/config/settings.json` (0600 — the file may hold secrets); at install time it is seeded with the defaults, an upgrade adds defaults for new keys without touching the user's choices. They are passed to the application (env/config file — per the template from the manifest).
- Changing settings — **`asc app settings <id>`**: an interactive editor in the terminal — pick a setting by number, enter a value, and it is validated against the definition (type, `limits`, enum `values`; secrets are masked in the list). Also via the platform UI. After a change the application is restarted (`asc app restart <id>`).

### 📏 Resource quota (quota)

The `quota:` section of `asc.settings.yaml` limits the resources of one app instance (DMN-021):

```yaml
quota:
  max_cpu: 2        # CPU cores limit (0.5, 2, …)
  max_ram: 1G       # memory limit: 512M, 2G, … (binary units, like docker -m)
  max_disk: 10G     # disk usage limit
```

- The values are normalized at install/upgrade time and recorded in `meta.json`; `asc app info <id>` shows them (`quota: cpu ≤ 2, ram ≤ 1.0 GiB, …`).
- **Docker apps**: enforced at container creation through the Engine API (`NanoCpus`, `Memory`).
- **native/process apps**: recorded in `meta.json`; cgroup enforcement is a next increment.
- `max_disk` is recorded for every runtime; per-runtime disk enforcement (Docker storage-opt / fs quotas) arrives incrementally.

### 🚀 The start command (start_command)

The application's start command is configured in `asc.settings.yaml` (it overrides `runtime.start` from `asc.yaml`). The string can **interpolate the application's environment variables** — `${VAR}` syntax:

```yaml
start_command: "java -Xmx${MEM_LIMIT}M -jar server.jar --port ${PORT} --level ${LOG_LEVEL}"
```

- The substitution is performed by the daemon at launch from the application's final environment (settings + org/node/application env levels, see [🌱 environments](../../../asc-platform/docs/features/environments.md)).
- An unresolved variable is a launch error naming the variable.
- The platform UI shows a computed preview of the command (secrets are masked).
- Changing the command works like changing settings: applied after an application restart.

### 🐳 install/update scripts: native or docker

A package's `install` and `update` scripts can run **either natively on the host or in docker** — controlled by the `run_in` field in the manifest's `scripts:` block:

```yaml
scripts:
  install:
    run: ./scripts/install.sh
    run_in: native            # native | docker
  update:
    run: ./scripts/update.sh
    run_in: docker            # a one-off container
    image: debian:12          # the image for docker execution (optional)
```

- `native` — the script runs on the host as the application's user.
- `docker` — the script runs in a one-off container with the `/asc/apps/<id>/` directory mounted (isolating build dependencies from the host).
- By default `run_in` inherits the package `type`: `docker` → docker, `native`/`utility` → native.

### Installation mechanics: cloning the repository

Installing an application = **cloning its repository**:

1. `asc install <package>` → the daemon clones the package repository into `/asc/apps/<id>/repository/`.
2. **Application versions = git tags** (GitHub tags): installing a specific version — `asc install <package>@1.2.0` (tag checkout), updating — `asc app upgrade <name>` (checkout of the new tag).
3. From then on the daemon works with the local copy: reads `asc.yaml`/`asc.settings.yaml`, builds/launches according to the application type.

**Updating** (`asc app upgrade <name>[@version]`, synonym — `asc upgrade`): the application must be stopped; the new tag is cloned **next to** the current copy (`repository.new`), the manifest is validated, and only then are the directories swapped and the runtime recreated (for Docker the container is recreated with the new image). A failure before the swap does not touch the installed application; a failure while recreating the runtime rolls back to the previous version. Without an explicit version, `latest` from the registry is used.

**Private repositories**: authorization is configured **per git host or prefix** (`github.com`, `github.com/myorg`) and stored separately from the source lists — `/etc/asc/git-auth.toml` (root) and `~/.config/asc/git-auth.toml` (user), both files 0600. Secrets end up neither in world-readable files nor in the argv of git processes (argv is visible to all users via /proc).

- **Methods**: a GitHub token — for `https://` URLs (passed via `GIT_ASKPASS` + an environment variable of the git process); an SSH key — for `git@`/`ssh://` URLs (`GIT_SSH_COMMAND` with `-i <key> -o IdentitiesOnly=yes`). When several entries match, the longest prefix wins; user entries take priority over system ones.
- **Detection**: git always runs with `GIT_TERMINAL_PROMPT=0` and `BatchMode=yes` — cloning a private repository without configured authorization does not hang on a password prompt but fails with a recognizable error (including the "Repository not found" that GitHub returns for private repositories without access).
- **Interactive setup**: on detecting a private repository, the CLI in a terminal **asks permission** to configure authorization right away: for https — token input, for ssh — a pick from the private keys found in `~/.ssh`; the choice is saved and the installation retries automatically. A non-interactive call (API, scripts) gets a structured error with an `asc auth add ...` hint.
- **CLI**: `asc auth add <host|prefix> --token <token>` · `asc auth add <host> --ssh-key [path]` (without a path — an interactive key picker) · `asc auth list` (methods without secrets) · `asc auth remove <host>`.

### Several applications in one repository: asc.stack.yaml

**The root rule**: a package repository may contain any number of `asc.yaml` files in subdirectories, but its root must hold **exactly one** manifest — either `asc.yaml` (a single application) or `asc.stack.yaml` (a stack) that ties all the nested `asc.yaml` files together. Nested manifests without a root one are not indexed.

The `asc.stack.yaml` stack manifest lists the applications and the paths to their `asc.yaml` files:

```yaml
name: my-stack
version: 1.0.0
description: "..."
apps:
  - name: web
    path: ./web            # the directory with asc.yaml
  - name: worker
    path: ./worker
  - name: db
    path: ./db
    optional: true          # an optional stack component
```

- `asc install my-stack` — install the whole stack; `asc install my-stack/web` — just one application from it.
- A stack can declare shared `env` and dependencies between applications (`depends_on` — the startup order); components can be `optional`.
- Registries and the platform store index both single `asc.yaml` files and `asc.stack.yaml` stacks.
- Examples — in the [asc-example-apps](../../../asc-example-apps) repository.

### Registries

- **Registry format** (the `registry` repo) — a hierarchy of JSON files: the root index `registry.json` → category files `categories/<topic>.json` (databases, ai, bots, game-servers, system-utilities, web…) → optional subcategories (`children`). Packages come in two kinds: `app` (asc.yaml) and `stack` (asc.stack.yaml). Validation schemas — in `registry/schema/`. Descriptions are in English.
- **Source tree**: the daemon's sourcelist → a tree is built from all registries (following the root index's `index`/`children` links), then a merged application list (per user); name conflicts are resolved by source priority.
- **Source types**: `file://` (a local directory) and `https://` (a registry, GitHub raw).
- **Per-user source lists**: two list levels —
  - **system** `/etc/asc/sources.toml` — managed by root (`sudo asc source add|remove`), the sources are visible to **all** users of the server;
  - **user** `~/.config/asc/sources.toml` — each user maintains their own list (`asc source add|remove` without sudo), which extends the system one.
  - The effective list = system sources (higher priority) + your own; a user cannot shadow or remove system sources (`asc source list` shows the origin of every source). Index caches: for root — in `data_dir`, for a user — in `~/.cache/asc/`.
- **Install policy** (`[policy]` in `/etc/asc/config.toml`, managed by root): `user_install = "all"` (default — users may install any packages: Docker, native, utilities) or `user_install = "docker"` (users may install Docker applications only; native apps and utilities are root-only). Applied at `asc install`; does not apply to root.
- **Index cache** with a TTL + `asc update` for a forced refresh.
- **CLI**: `asc install|remove|upgrade <pkg>`, `asc search <query>`, `asc source add|remove|list`, `asc update`.

## 🔗 Related tasks

DMN-003, DMN-018, REG-001, REG-002, BE-002, BE-003 in [ROADMAP.md](../../../asc-platform/ROADMAP.md); GRW-011 in [ROADMAP-GROWTH.md](../../../asc-platform/ROADMAP-GROWTH.md).
