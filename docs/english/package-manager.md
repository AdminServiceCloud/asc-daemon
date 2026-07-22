# 📦 Package manager and registries

## 📌 Description

A package manager in the spirit of apt/homebrew: applications are described by an `asc.yaml` manifest, published in registries and installed with `asc install <package>`. Registries can be official or custom, local (`file://`) or remote (`https://`), including GitHub repositories.

## 🎯 Scenarios

- `asc install nginx` — install from the official registry.
- `asc source add https://registry.example.com` — connect a custom registry (like an apt source).
- `asc source add https://github.com/user/my-app` — an application straight from GitHub (asc.yaml at the root); for a private repo a 404 triggers an offer to configure a token.
- `asc install https://github.com/user/my-app --branch dev` — install directly from a repository URL, no registry involved at all (DMN-040): a one-off, for forks and packages that were never published. `asc source add` above wires a repo into the registry system permanently; this is for trying one out.
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

#### 🏗️ Local image build: `image-build` (DMN-050)

A `type: docker` app can ship its own **Dockerfile** and have the Engine build the image locally instead of (or beside) pulling a prebuilt one:

```yaml
runtime:
  image-build:
    context: .              # build context dir, relative to the manifest (default '.')
    dockerfile: Dockerfile  # relative to the context (default 'Dockerfile')
    args:                   # optional --build-arg values
      VERSION: "1.0"
    tag: asc-local/my-app   # optional; default 'asc-local/<app>:latest'
```

- **Only `image`** → pull it, as before.
- **Only `image-build`** → build the image from the package Dockerfile at install (and rebuild it on upgrade / a settings-drift recreate; layer caching keeps this cheap). The build context is packed from the package repository and never reaches outside it.
- **Both `image` and `image-build`** → the installer offers a **choice**: interactively `asc install <app>` prints the two options and asks; non-interactively (or to skip the prompt) pass **`--image`** (pull the prebuilt one) or **`--build`** (build locally). The chosen source is recorded in `meta.json` so a later recreate or upgrade uses the same one without asking again.

> ⚠️ The build runs as the daemon (root). Until per-user container policy lands (DMN-043), building is intended for trusted packages; the base image referenced by `FROM` is pulled by the Engine anonymously (private base images for a local build are a later increment).

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

  - key: game_version
    type: enum                  # presets, but a custom value is also accepted
    values: [public, latest_experimental]
    default: public
    allow_custom: true           # any other branch/build id is accepted as-is

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
    protocol: both               # tcp (default) | udp | both

  - key: game_data
    type: volumes               # app volumes (a list, forms below)
    default: [/home/steam/cs2-dedicated]
```

- Setting values are saved in `/asc/apps/<id>/config/settings.json` (0600 — the file may hold secrets); at install time it is seeded with the defaults, an upgrade adds defaults for new keys without touching the user's choices.
- **Env pass-through**: every setting that declares `env: VAR_NAME` lands in the application's environment (secrets included — that is what their `env:` is for). For Docker apps the variables go into the container env at creation. List values (`ports`, `volumes`) are exposed comma-joined.
- **`type: ports`** — the published container ports (host port == container port). Declaring the same setting with `env:` keeps the app and Docker in sync: the server listens exactly where the port is forwarded. **`protocol`** picks the transport(s) to forward: `tcp` (default), `udp`, or `both` (the same host==container port on TCP and UDP).
- **`type: volumes`** — the app's volumes; every entry takes one of three forms:
  - `/container/path` — private app data: the app's **data folder** (`/asc/apps/<id>/data`) is mounted at that container path. The folder is created world-writable (0777): images run under arbitrary non-root users and bind mounts keep host ownership; the app directory above it stays restrictive. When the image declares a numeric `USER uid:gid`, the folder is also chowned to it (DMN-038) — a non-root process may only chown a path it already owns, so an image that `chown`s its own data directory on first start (not just writes to it) needs this to avoid EPERM; a named (`steam`) or bare-uid `USER` is left as world-writable only, since its group is only known to the image's own `/etc/passwd`;
  - `/container/path:host` — same, but the host side after the colon is used **instead of `data`**: a plain folder name lands inside the app directory (`/asc/apps/<id>/<folder>`; `repository`, `config` and `meta.json` are reserved), an **absolute path** is a host machine path mounted verbatim (a pre-existing directory keeps its ownership and mode);
  - `name:/container/path[:ro|:rw]` — a Docker **named volume**, created by the Engine on first use. Named volumes are how several apps share data: one app writes the volume, others mount it `:ro` (see the cs2 stack in [asc-example-apps](../../../asc-example-apps)). Named volumes are not removed with the app.
- **Applying changes**: a container's configuration is fixed at creation, so on the next `asc app start` / `asc app restart` the daemon compares the desired state — env, published ports, volumes, quota, start command — with the container's actual one and **recreates the container** when they differ (or when the container is missing). App data lives in volumes and survives the recreate. If the desired state cannot be computed (say, the registry source is gone), the app still starts as is — availability wins, with a warning in the log.
- Changing settings — **`asc app settings <id>`**: an interactive editor in the terminal. It first shows the **categories** — `environments` (string/number/boolean/enum/secret settings), `ports`, `volumes`, `quota`, `start_command` — then the settings of the picked category: pick one by number, enter a value, and it is validated against the definition (type, `limits`, enum `values`; secrets are masked in the list; ports and volumes take space-separated lists). Also via the platform UI. After a change the application is restarted (`asc app restart <id>`).
- **`allow_custom` (type: enum only)** — accepts any value outside the declared `values` list as free text, instead of rejecting it. Use it for enums that list common presets but should not lock the user out of a value the author didn't anticipate (a game branch/build id, a custom world name). The numbered picker in `asc app settings` still lists the presets; typing anything else is simply accepted.

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
2. **Application versions = git tags** (GitHub tags), read from the **repository**, not the registry (DMN-047): installing a specific version — `asc install <package>@1.2.0` (tag checkout); `asc install <package>` with no version resolves the repository's **newest tag** via `git ls-remote` (no tags → the default branch HEAD); `asc install <package>@` (a bare `@`) lists the repository's tags and branches for an interactive pick (DMN-048), or, non-interactively, returns the available versions as an error. Updating — `asc app upgrade <name>` (checkout of the new tag). The registry index therefore carries **no version field** — a package's versions are whatever its repository is tagged with.
3. **License consent** (DMN-028, DMN-032): when the cloned package ships a license (`LICENSE.md` / `LICENSE` / `LICENSE.txt`), the CLI shows where the package comes from (registry source + repository), prints the license text and asks for acceptance — declining aborts the install and leaves nothing behind. The license is looked up in the **package's own directory** first (a monorepo package may ship its own license), falling back to the repository root. Non-interactive input accepts automatically with a printed notice; API callers receive a structured error with the source, the repository and the license text (the platform UI renders its own consent dialog). A stack asks once per repository. Packages without a license file install without the prompt.
4. **Custom name and multiple instances** (DMN-024, DMN-033): in a terminal `asc install` asks for an application name — Enter keeps the default, any other input becomes the app's name. Non-interactively, pass `--name "My Server"`. Installing a package that is already installed no longer fails: it becomes a **new instance** with the next free id (`<package>-2`, `<package>-3`, …), which also becomes its `custom_name` unless `--name` overrides it. The name is stored in `meta.json` (`custom_name`), must be unique among the user's apps, survives upgrades, and every command accepts it interchangeably with the id. A suffixed instance records the registry package it came from (`meta.package`), so `asc app upgrade` keeps resolving it correctly.
5. From then on the daemon works with the local copy: reads `asc.yaml`/`asc.settings.yaml`, builds/launches according to the application type.
6. For Docker applications the container is created through the Engine API; an image missing on the host is **pulled automatically** from its registry (`runtime.image`; a name without a tag means `latest`) — both at install and at upgrade.
7. **Progress bars**: on a terminal, the repository clone (`git clone --progress`) and a Docker image pull both render as live bars (`docker pull`/`docker-compose pull` style — one bar per image layer) — on by default, independent of `asc config debug`. Non-interactive callers (piped output, the daemon API) get none of this; the same events are always available as `debug`-level tracing (`asc config debug on`).

**Requirements at start** (DMN-029): the manifest's `requirements` (`ram`, `disk`, `cpu`) are compared with what the host has free when the app starts. When short, `asc app start` warns with the exact figures and — in a terminal — asks whether to start anyway at the user's own risk; non-interactive callers get the warning on stderr and proceed. The check is advice, not enforcement: read failures never block the start.

**Updating** (`asc app upgrade <name>[@version]`, synonym — `asc upgrade`): the application must be stopped; the new tag is cloned **next to** the current copy (`repository.new`), the manifest is validated, and only then are the directories swapped and the runtime recreated (for Docker the container is recreated with the new image). A failure before the swap does not touch the installed application; a failure while recreating the runtime rolls back to the previous version. Without an explicit version, the repository's newest tag is used (DMN-047); a repository with no tags cannot be upgraded without an explicit `@version`.

**Direct install from a git repository** (`asc install <url> [--branch <name>|--tag <name>]`, DMN-040): when the spec is a git URL (`https://`, `ssh://`, or the scp-like `git@host:path`) rather than a package name, the daemon skips the registry entirely and clones the repository straight in — the same clone/manifest/provision pipeline as a registry install, minus the resolution step. `asc.yaml` must be at the repository root (no monorepo `path`, since there is no registry entry to carry one). `--branch`/`--tag` pick the ref to check out; neither flag clones the default branch HEAD. The app id defaults to the repository's own name (`bar` for both `https://github.com/foo/bar.git` and `git@github.com:foo/bar`); `--name` overrides it, same as a registry install. Private repositories reuse the exact same `asc auth` credentials below — a URL install is just a registry install with the resolution step removed. `asc upgrade` does not yet resolve apps installed this way (there is no registry entry to re-resolve against); re-running `asc install` with the same URL creates a new instance instead of upgrading in place.

**Credentials** (DMN-045/046): one per-user store holds authorization both for private **repositories** (`git clone`) and for private image **registries** (the Engine pull), told apart by a `type` field (`repo` | `registry`). Entries are keyed **per host or prefix** (`github.com/myorg`, `ghcr.io/myorg`) and stored separately from the source lists, as JSON: `/etc/asc/auth.json` (root) and `~/.asc/auth.json` (user, alongside the rest of the per-user DMN-041 tree), both files 0600. Secrets end up neither in world-readable files nor in the argv of git processes (argv is visible to all users via /proc).

```json
{
  "credentials": [
    { "type": "repo",     "pattern": "github.com/myorg", "token": "ghp_xxx" },
    { "type": "registry", "pattern": "ghcr.io/myorg", "username": "me", "token": "ghp_yyy" },
    { "type": "repo",     "pattern": "github.com/myorg/secret-app", "app": "6f8a…uuid", "key": "/home/me/.ssh/id_ed25519" }
  ]
}
```

- **Types**: `repo` authorizes a git clone — a token for `https://` URLs (via `GIT_ASKPASS` + an environment variable of the git process) or an SSH key for `git@`/`ssh://` URLs (`GIT_SSH_COMMAND` with `-i <key> -o IdentitiesOnly=yes`). `registry` authorizes an image pull — a **token plus username**, sent to the Docker Engine as the `X-Registry-Auth` header (the Engine, not the daemon, then contacts the registry, so no TLS stack is needed daemon-side). A registry entry rejects an SSH key and requires `--username`.
- **App binding** (`app`, DMN-044): a credential may be scoped to a single application by its uuid or id (`--app`), so a token serves exactly the app that needs it and no other. An app-bound entry is invisible to every other app and beats an equally specific unbound one for its own app; leaving `app` unset applies the credential to every app whose URL/image matches the pattern. Matching otherwise follows the same rule for both types: the longest prefix at a path boundary wins, and user entries take priority over system ones.
- **Detection**: git always runs with `GIT_TERMINAL_PROMPT=0` and `BatchMode=yes` — cloning a private repository without configured authorization does not hang on a password prompt but fails with a recognizable error (including the "Repository not found" that GitHub returns for private repositories without access). A private image pull surfaces the Engine's own authorization error.
- **Interactive setup**: on detecting a private repository, the CLI in a terminal **asks permission** to configure authorization right away: for https — token input, for ssh — a pick from the private keys found in `~/.ssh`; the choice is saved and the installation retries automatically. A non-interactive call (API, scripts) gets a structured error with an `asc auth add ...` hint.
- **Migration**: the pre-DMN-045 TOML files (`/etc/asc/git-auth.toml`, `~/.config/asc/git-auth.toml`) are still read when no JSON store exists — their entries have no `type` and load as `repo` — and are migrated to `auth.json` on the next `asc auth` write, so configured auth keeps working across the upgrade.
- **CLI**: `asc auth add <host|prefix> [--type repo|registry] --token <token> [--username <user>] [--app <uuid|id>]` · `asc auth add <host> --ssh-key [path]` (without a path — an interactive key picker) · `asc auth list` (types and methods without secrets) · `asc auth remove <host> [--type repo|registry]`.

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
- **Renaming a stack at install** (DMN-034): `asc install my-stack --name prod` is a **prefix** applied to every installed app — `prod-web`, `prod-worker`, `prod-db` — since a stack has no entity of its own, only its member apps. `asc install my-stack/web --name prod-web` names just the requested app, as for a single-app install.
- A stack can declare dependencies between applications (`depends_on` — the startup order); components can be `optional`. Environment variables live in each app's own `asc.settings.yaml`.
- Registries and the platform store index both single `asc.yaml` files and `asc.stack.yaml` stacks.
- Examples — in the [asc-example-apps](../../../asc-example-apps) repository.

**Stack install mechanics**: the repository is cloned once to read `asc.stack.yaml`, then every selected application installs like a regular app — with its own repository clone, `/asc/apps/<id>/` directory and meta.json:

- **The app id** is the `name` from the app's own `asc.yaml` (in the example above the stack's `web` app may be named `my-stack-web`); the origin is recorded in meta.json as `package: "my-stack/web"` — `asc app upgrade` resolves the package through it.
- **Order**: dependencies (`depends_on`) install first; cycles are rejected at validation. `asc install my-stack` installs every non-`optional` component; `asc install my-stack/db` installs the requested component (even an `optional` one) plus its dependencies.
- **Repeat installs**: a wanted app (the one(s) requested, not a dependency pulled in alongside them) that is already installed becomes a **new instance** (DMN-033) instead of being skipped; a dependency that is already installed is still reused, not duplicated. Every app installs atomically (a failure removes only its own directory, previously installed components stay).
- **License consent** is asked once per repository (one repo = one license), not per stack app.

### Registries

- **Registry format** (the `registry` repo) — a hierarchy of JSON files: the root index `registry.json` → category files `categories/<topic>.json` (databases, ai, bots, game-servers, system-utilities, web…) → optional subcategories (`children`). Packages come in two kinds: `app` (asc.yaml) and `stack` (asc.stack.yaml). Validation schemas — in `registry/schema/`. Descriptions are in English.
- **Source tree**: the daemon's sourcelist → a tree is built from all registries (following the root index's `index`/`children` links), then a merged application list (per user); in search results name conflicts are resolved by source priority.
- **Name conflicts at install**: when several sources provide the requested package, `asc install` in a terminal lists the candidates (source name + repository) and asks which one to use (pick a number); non-interactive callers get an error with the same list — pin the registry explicitly: `asc install <pkg> --source <name>` (API: the `source` field of InstallAppRequest). `asc app upgrade` prefers the source the app was installed from.
- **Source types**: `file://` (a local directory) and `https://` (a registry, GitHub raw).
- **Fetch resilience** (DMN-036): each registry index file (`registry.json`, category files) is a separate `curl` invocation with no connection reuse between them — a whole `asc update`/`asc search` run is a short burst of several small HTTPS requests to the same host. A stalled connection on any one of them (CDN throttling, transient network hiccup) used to hang the whole command for up to 5 minutes with zero output. Registry fetches now use a short per-request timeout (20s) plus a couple of retries on transient errors, instead of the generous 300s budget reserved for large downloads (`asc-updater` release assets).
- **`asc update` progress** (DMN-037): on a terminal, every index file gets its own progress line, `docker pull`/`git clone` style — a spinner while the request is in flight, frozen on its byte count on success or the error on failure. Since the registry is fetched one small file at a time, this is what turns a stuck fetch into a visibly stuck spinner instead of a silent wait.
- **Per-user source lists**: two list levels —
  - **system** `/etc/asc/sources.toml` — managed by root (`sudo asc source add|remove`), the sources are visible to **all** users of the server;
  - **user** `~/.config/asc/sources.toml` — each user maintains their own list (`asc source add|remove` without sudo), which extends the system one.
  - The effective list = system sources (higher priority) + your own; a user cannot shadow or remove system sources (`asc source list` shows the origin of every source). Index caches: for root — in `data_dir`, for a user — in `~/.cache/asc/`.
- **Install policy** (`[policy]` in `/etc/asc/config.toml`, managed by root): `user_install = "all"` (default — users may install any packages: Docker, native, utilities) or `user_install = "docker"` (users may install Docker applications only; native apps and utilities are root-only). Applied at `asc install`; does not apply to root.
- **Index cache** with a TTL + `asc update` for a forced refresh.
- **CLI**: `asc install|remove|upgrade <pkg>` (or `asc install <git-url> [--branch|--tag]`), `asc search <query>`, `asc source add|remove|list`, `asc update`.

## 🔗 Related tasks

DMN-003, DMN-018, DMN-038, DMN-040, DMN-045, DMN-046, DMN-047, DMN-048, REG-001, REG-002, BE-002, BE-003 in [ROADMAP.md](../../../asc-platform/ROADMAP.md); GRW-011 in [ROADMAP-GROWTH.md](../../../asc-platform/ROADMAP-GROWTH.md).
