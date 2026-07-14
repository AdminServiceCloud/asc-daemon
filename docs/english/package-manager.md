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
  stdin: true                 # docker, optional: keep stdin open (docker run -i) — `asc attach` input reaches the app
  tty: true                   # docker, optional: allocate a pseudo-TTY (docker run -t)
  # or install/start/stop commands for native
requirements: { ram: 256M, disk: 1G }
healthcheck: { http: /health }
```

> ℹ️ The manifest has **no `env:`, `ports:` or `volumes:` sections** (DMN-027/030): environment variables, published ports and volumes are all declared in `asc.settings.yaml` — settings with an `env:` key and the `ports` / `volumes` setting types (see below). One source of truth: what the user can configure is exactly what the app gets.

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
    env: SERVER_NAME            # exposed to the app as this env variable

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

  - key: game_port
    type: ports                 # published container ports (a list)
    default: [27015]
    limits: { min: 1024, max: 65535 }
    env: CS2_PORT               # exposed comma-joined; one port — as is

  - key: game_data
    type: volumes               # app volumes (a list, forms below)
    default: [/home/steam/cs2-dedicated]
```

- Setting values are saved in `/asc/apps/<id>/config/settings.json` (0600 — the file may hold secrets); at install time it is seeded with the defaults, an upgrade adds defaults for new keys without touching the user's choices.
- **Env pass-through**: every setting that declares `env: VAR_NAME` lands in the application's environment (secrets included — that is what their `env:` is for). For Docker apps the variables go into the container env at creation. List values (`ports`, `volumes`) are exposed comma-joined.
- **`type: ports`** — the published container ports (host port == container port). Declaring the same setting with `env:` keeps the app and Docker in sync: the server listens exactly where the port is forwarded.
- **`type: volumes`** — the app's volumes; every entry takes one of three forms:
  - `/container/path` — private app data: the app's **data folder** (`/asc/apps/<id>/data`) is mounted at that container path. The folder is created world-writable (0777): images run under arbitrary non-root users and bind mounts keep host ownership; the app directory above it stays restrictive. Per-app uid mapping will tighten this later;
  - `/container/path:host` — same, but the host side after the colon is used **instead of `data`**: a plain folder name lands inside the app directory (`/asc/apps/<id>/<folder>`; `repository`, `config` and `meta.json` are reserved), an **absolute path** is a host machine path mounted verbatim (a pre-existing directory keeps its ownership and mode);
  - `name:/container/path[:ro|:rw]` — a Docker **named volume**, created by the Engine on first use. Named volumes are how several apps share data: one app writes the volume, others mount it `:ro` (see the cs2 stack in [asc-example-apps](../../../asc-example-apps)). Named volumes are not removed with the app.
- **Applying changes**: a container's configuration is fixed at creation, so on the next `asc app start` / `asc app restart` the daemon compares the desired state — env, published ports, volumes, quota, start command — with the container's actual one and **recreates the container** when they differ (or when the container is missing). App data lives in volumes and survives the recreate. If the desired state cannot be computed (say, the registry source is gone), the app still starts as is — availability wins, with a warning in the log.
- Changing settings — **`asc app settings <id>`**: an interactive editor in the terminal. It first shows the **categories** — `environments` (string/number/boolean/enum/secret settings), `ports`, `volumes`, `quota`, `start_command` — then the settings of the picked category: pick one by number, enter a value, and it is validated against the definition (type, `limits`, enum `values`; secrets are masked in the list; ports and volumes take space-separated lists). Also via the platform UI. After a change the application is restarted (`asc app restart <id>`).

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
- **User overrides** (DMN-030): the `quota` category of `asc app settings` overrides individual fields on top of the package values (`'-'` resets a field back); the override lands in `settings.json` and applies through the container recreate on the next restart.

### 🚀 The start command (start_command)

The application's start command is configured in `asc.settings.yaml`. The string can **interpolate the package's environment variables** — `${VAR}` syntax:

```yaml
start_command: "steamcmd +force_install_dir /data +login anonymous +app_update ${STEAM_APP_ID} validate +quit"
```

- The substitution is performed by the daemon at install/upgrade time from the app's env — the setting values with `env:` keys, defaults included. An unresolved variable fails the install naming the variable.
- **Docker apps**: the command replaces what the image would run (the entrypoint becomes `/bin/sh -c`, so arguments and quoting work as in a shell).
- **native apps**: the command overrides `runtime.start` from `asc.yaml`.
- Interpolation from the application's *final* environment (org/node/application env levels — [🌱 environments](../../../asc-platform/docs/features/environments.md)) and a UI preview of the computed command are a next increment; setting values already reach the app as env variables (see the settings section above).
- **User override** (DMN-030): the `start_command` category of `asc app settings` replaces the package's command for this instance (`'-'` resets back); `${VAR}` references resolve from the settings env when the override is applied.

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
3. **License consent** (DMN-028): when the cloned repository ships a license (`LICENSE.md` / `LICENSE` / `LICENSE.txt` at its root), the CLI shows where the package comes from (registry source + repository), prints the license text and asks for acceptance — declining aborts the install and leaves nothing behind. Non-interactive input accepts automatically with a printed notice; API callers receive a structured error with the source, the repository and the license text (the platform UI renders its own consent dialog). A stack asks once per repository. Repositories without a license file install without the prompt.
4. **Custom name** (DMN-024): in a terminal `asc install` asks for an application name — Enter keeps the default (the name from the package manifest), any other input becomes the app's name. Non-interactively, pass `--name "My Server"`. The name is stored in `meta.json` (`custom_name`), must be unique among the user's apps, survives upgrades, and every command accepts it interchangeably with the id. A custom name applies to a single application — not to a whole stack.
5. From then on the daemon works with the local copy: reads `asc.yaml`/`asc.settings.yaml`, builds/launches according to the application type.
6. For Docker applications the container is created through the Engine API; an image missing on the host is **pulled automatically** from its registry (`runtime.image`; a name without a tag means `latest`) — both at install and at upgrade.

**Requirements at start** (DMN-029): the manifest's `requirements` (`ram`, `disk`, `cpu`) are compared with what the host has free when the app starts. When short, `asc app start` warns with the exact figures and — in a terminal — asks whether to start anyway at the user's own risk; non-interactive callers get the warning on stderr and proceed. The check is advice, not enforcement: read failures never block the start.

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
- A stack can declare dependencies between applications (`depends_on` — the startup order); components can be `optional`. Environment variables live in each app's own `asc.settings.yaml`.
- Registries and the platform store index both single `asc.yaml` files and `asc.stack.yaml` stacks.
- Examples — in the [asc-example-apps](../../../asc-example-apps) repository.

**Stack install mechanics**: the repository is cloned once to read `asc.stack.yaml`, then every selected application installs like a regular app — with its own repository clone, `/asc/apps/<id>/` directory and meta.json:

- **The app id** is the `name` from the app's own `asc.yaml` (in the example above the stack's `web` app may be named `my-stack-web`); the origin is recorded in meta.json as `package: "my-stack/web"` — `asc app upgrade` resolves the package through it.
- **Order**: dependencies (`depends_on`) install first; cycles are rejected at validation. `asc install my-stack` installs every non-`optional` component; `asc install my-stack/db` installs the requested component (even an `optional` one) plus its dependencies.
- **Idempotency**: already-installed apps of the stack are skipped and left untouched; every app installs atomically (a failure removes only its own directory, previously installed components stay).
- **License consent** is asked once per repository (one repo = one license), not per stack app.

### Registries

- **Registry format** (the `registry` repo) — a hierarchy of JSON files: the root index `registry.json` → category files `categories/<topic>.json` (databases, ai, bots, game-servers, system-utilities, web…) → optional subcategories (`children`). Packages come in two kinds: `app` (asc.yaml) and `stack` (asc.stack.yaml). Validation schemas — in `registry/schema/`. Descriptions are in English.
- **Source tree**: the daemon's sourcelist → a tree is built from all registries (following the root index's `index`/`children` links), then a merged application list (per user); in search results name conflicts are resolved by source priority.
- **Name conflicts at install**: when several sources provide the requested package, `asc install` in a terminal lists the candidates (source name + repository) and asks which one to use (pick a number); non-interactive callers get an error with the same list — pin the registry explicitly: `asc install <pkg> --source <name>` (API: the `source` field of InstallAppRequest). `asc app upgrade` prefers the source the app was installed from.
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
