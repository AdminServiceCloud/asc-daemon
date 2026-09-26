# 🌐 Web server (daemon)

> 🌍 **Language:** English · [🇷🇺 Русская версия](../russian/webserver.md)

## 📌 Description

The `webserver` module installs a web server on the node and manages it. The first engine is **nginx**. Through it the daemon publishes apps on domains (reverse proxy), issues and renews Let's Encrypt certificates on its own, accepts ready-made certificates, and can pass visitors' real IPs through Cloudflare. Everything works standalone through `asc web …`. The AdminService.Cloud platform uses the same API and distributes sites as desired state ([🌐 web-server](../../../asc-platform/docs/features/web-server.md)).

Only the system (root) daemon manages a web server.

## 🎯 Scenarios

- 🧰 `sudo asc web install --mode system` installs nginx from the nginx.org repository (or the distribution's package when nginx.org has no build for the release), or adopts the one already installed. `--mode docker` starts an `asc-webserver` container.
- 🚀 `asc web site add grafana.example.com --app grafana --port 3000 --tls letsencrypt` — a minute later `https://grafana.example.com` works, and the certificate then renews itself.
- 🔗 `asc web site add api.example.com --to 127.0.0.1:8080` proxies to a service that is not an ASC app.
- ☁️ `asc web site add … --cloudflare` hands the app real visitor IPs instead of Cloudflare's addresses.
- 🧪 `asc web test` shows the `nginx -t` output for what would be applied now; `asc web reload` re-renders the configuration, checks it and reloads nginx.
- 🛡️ The platform sends a site with a typo in `extra_server`. `nginx -t` fails; the daemon applies the other sites, keeps **the previous working version** of this one and marks it `error` with the error text.

## 🏗️ Technical design

Code: `src/daemon/webserver/`:

| File | What it does |
|---|---|
| `mod.rs` | the `WebServer` manager: stores, API operations, background pass |
| `model.rs` | settings, sites, statuses, and validation of everything that ends up in directives |
| `render.rs` | generation of `nginx.conf`, snippets and site files (pure functions) |
| `apply.rs` | staging → `nginx -t` → swap → reload, isolation of a broken site |
| `engine.rs` | installing and driving nginx in `system` and `docker` modes |
| `acme.rs` | Let's Encrypt (HTTP-01) with a domain self-check |
| `cert.rs` | certificate expiry, issuer, SANs; key/certificate pair check |
| `cloudflare.rs` | Cloudflare ranges for `real_ip` |

API: `src/daemon/api/webserver.rs` (gRPC `WebServerService` and the REST routes for the CLI).

### 🧰 Install modes

| Mode | Install | Process control |
|---|---|---|
| `system` | apt (Debian/Ubuntu): the nginx.org repository (`signed-by` on the ASCII-armored key, pinned to the nginx.org origin); the distribution's package when there is no build for the release. dnf/yum (RHEL family): the nginx.org repository with fallback to the distribution's package. An nginx that is already installed is **adopted**: the original `nginx.conf` is saved once to `nginx.conf.asc-orig` | `systemctl reload-or-restart nginx`, status from `systemctl is-active` |
| `docker` | Container `asc-webserver` from `nginx:stable-alpine` (image configurable), `network_mode: host`, `restart: unless-stopped`, label `asc.managed=webserver`; bollard only, no `docker` CLI | `SIGHUP` to the container, `nginx -t` via exec |

**Adoption (`system`).** The original `nginx.conf` keeps its `user` and `pid` (the latter must match the systemd unit's `PIDFile`), Debian's dynamic-module include, `conf.d/*.conf` and `sites-enabled/*`. The daemon never edits the operator's files. If they already declare a `default_server` on `:80` or `:443` (like Debian's `sites-enabled/default`), the daemon simply does not generate its own. On uninstall, an adopted nginx gets its original `nginx.conf` back and stays installed. With SELinux in Enforcing mode, `httpd_can_network_connect` is turned on. For an active ufw/firewalld the daemon only prints a hint and leaves the firewall alone.

**Docker.** Before starting, the daemon checks that ports 80/443 are free. nginx runs as PID 1 (the image's entrypoint is replaced): the image's scripts rewrite a `default.conf` nothing reads here, and a `SIGHUP` arriving while they run would hit the shell. The configuration is passed with `-c /etc/asc/webserver/nginx.conf` rather than by mounting a single file, since an atomic `rename` swap would be invisible inside the container. `/etc/asc/webserver`, `/var/www/asc-acme` and `/var/lib/asc/webserver/acme/certs` are mounted read-only **at the same paths** as on the host, so the generated configuration is identical in both modes.

### ⚙️ Settings — `/etc/asc/webserver/webserver.toml`

```toml
mode = "system"             # system | docker; written by install
image = "nginx:stable-alpine"
worker_processes = "auto"
worker_connections = 4096
keepalive_timeout = 65
client_max_body_size = "64m"
gzip = true
gzip_level = 5
server_tokens = false
http2 = true
tls_protocols = ["TLSv1.2", "TLSv1.3"]
hsts = false
cloudflare_real_ip = false  # true — trust Cloudflare for every site (http level)
access_log = true
acme_email = "ops@example.com"
acme_directory = ""         # empty — Let's Encrypt production
custom_main = ""            # inserted into the main context
custom_http = ""            # inserted into http {}

[host]                      # host facts captured at install time
user = "www-data"
pid = "/run/nginx.pid"
```

Changed through the API (`UpdateWebServerSettings`): new settings go through `nginx -t` first and are saved only if it passes.

### 🧱 Files

```
/etc/asc/webserver/
├── webserver.toml
├── nginx.conf                          # system: written to /etc/nginx/nginx.conf
├── http.d/custom.conf                  # custom_http
├── snippets/acme.conf                  # location ^~ /.well-known/acme-challenge/
├── snippets/proxy.conf                 # X-Real-IP, X-Forwarded-*
├── snippets/cloudflare-realip.conf
├── sites/<id>.conf
└── certs/<id>/{fullchain.pem,privkey.pem}      # provided certificates, key 0600
/var/lib/asc/webserver/
├── sites.json                          # sites (desired state), 0600
├── status.json                         # site statuses and the last apply
├── cloudflare.json                     # the last downloaded range list
└── acme/
    ├── account-<hash>.json             # one ACME account per directory, 0600
    └── certs/<id>/{fullchain.pem,privkey.pem}
/var/www/asc-acme/.well-known/acme-challenge/   # HTTP-01 webroot, readable by the nginx worker
```

`nginx.conf` has a `default_server` for `:80` (the ACME snippet, everything else `return 444`) and for `:443` (`ssl_reject_handshake on`, nginx 1.19.4+), so unknown names never get whichever site happens to be first. Syntax follows the version: `http2 on;` from 1.25.1, `listen … http2` before it. `listen [::]` is generated only when the host has IPv6.

### 🔁 Applying configuration

1. Resolve every site: upstream addresses (app → `127.0.0.1:<host port>`), the certificate path, name clashes. Local sites claim their names first, then the rest in id order, so a clash always rejects the same site.
2. Render the full set of files into `/etc/asc/webserver/.staging/` and run `nginx -t -c .staging/nginx.conf`.
3. If the output names a specific site's file or certificate, that site falls back to its previous live file (or is left out when it has none) and the check runs again. A failure in the node-wide configuration aborts everything and leaves the live files untouched.
4. Render the set again with live paths, write every file through a temporary file and `rename`, delete files of sites that are gone, reload.
5. Record statuses: `applied`, `error` (with the `nginx -t` output), `disabled`, `pending` (web server not installed).

Applies are serialized by a mutex. A Let's Encrypt site with no certificate yet is rendered on `:80` only. Once the certificate is issued, the apply repeats and adds `:443` and the redirect.

### 🧩 Site

```text
Site
  id              [a-z0-9][a-z0-9_.-]{0,63} (the platform uses the domain id)
  server_names    plain and wildcard (*.example.com) names, no regex
  managed_by      absent — local; "platform" — the platform's sites
  disabled
  upstream        servers[]: app{app, port} | address "host:port"; weight, backup,
                  max_fails, fail_timeout, down; balance: round_robin|least_conn|ip_hash|hash;
                  hash_key; keepalive; tls (HTTPS to the upstream)
  tls             mode none|acme|provided; certificate_pem/private_key_pem (provided only,
                  never returned through the API); redirect_http, hsts, http2
  proxy           websocket, client_max_body_size, connect/read/send timeouts,
                  request_headers[], response_headers[], upstream_host, no_buffering
  real_ip         off | cloudflare
  extra_server, extra_location     text pasted into server {} and location / {}
  raw_config      the site's full text — rendered as is
```

An `app{app, port}` member is an app id, name or uuid plus a container port. The daemon resolves it to the published host port. The background pass notices when the app is recreated with another port and re-renders the site. `backup` is silently dropped for `ip_hash`/`hash`, since nginx does not allow it there.

Header values may not contain quotes, `;`, `{`, `}`, backslashes or control characters. `extra_*`, `raw_config` and `custom_*` are arbitrary nginx directives, so in what they allow they amount to root access to the node: the platform grants them only to users who may manage the node.

`ReplaceSites(managed_by, sites)` replaces **only** the sites with the same `managed_by`. Local CLI sites and platform sites never overwrite each other, and the same id under two owners is refused.

### 🔐 Let's Encrypt (ACME)

- The `instant-acme` crate on rustls + ring (the same TLS stack as the API), **HTTP-01** challenge via the webroot.
- **Self-check.** Every name is checked before an order. A name that does not resolve gets `pending_dns` ("create an A/AAAA record"). A name answered by someone else (other content or status) gets `pending_dns` with the addresses it points at. Either way no order is created, so no Let's Encrypt quota is spent. A self-check that cannot connect (hairpin NAT) is inconclusive: the order goes ahead and Let's Encrypt decides.
- States: `pending_dns` → `issuing` → `active` → `expiring` (under 30 days left) / `error` (ACME text).
- Backoff after failures: 1 h, 2 h, 4 h … up to 24 h. `asc web site renew <id>` orders right away, ignoring the backoff.
- The certificate key is generated on the node and never leaves it. The account is created once per ACME directory, with `acme_email` as the contact.
- `acme_directory` can be overridden: Let's Encrypt staging for tests, or another ACME provider.

### 🩺 Health checks (DMN-126)

A site's upstream can carry an active check: `health_check` of kind `tcp` (a connect) or `http` (a GET of a path, an expected status or any 2xx/3xx), with an interval (5 s), a timeout (2 s), failures to leave rotation (3) and passes to return (2). The daemon probes every server directly, not through nginx, so a server taken out keeps being probed and comes back on its own. A failing server gets `down` in the upstream; changes are applied through `nginx -t` and a reload at most once every 3 seconds. When every server fails, none is taken out: an empty upstream is worse than letting nginx try. State lives in memory and is reported in the site status (`upstream_health`: healthy or not, last error, latency). HTTPS checks do not verify certificates, since servers are addressed by IP.

`ProbeTcp` is a one-off TCP connect from the node to an address — how the platform checks that a load balancer member on another node is reachable from the balancer node.

### 🔄 Background pass

Every 10 minutes, and right after sites with Let's Encrypt change:

- refresh the Cloudflare ranges once a day;
- check whether an upstream app's host port has changed;
- issue or renew certificates that have no file, whose names changed or that have under 30 days left, when the backoff allows.

### ☁️ Cloudflare real IP

`snippets/cloudflare-realip.conf` holds `set_real_ip_from` for every Cloudflare range, `real_ip_header CF-Connecting-IP` and `real_ip_recursive on`. The list is refreshed daily from `https://www.cloudflare.com/ips-v4` and `/ips-v6`. Every line is validated as a CIDR, and any invalid line discards the whole response, so an error page or captive portal never becomes the list of trusted proxies. A fallback list is embedded in the binary. The snippet is included in `http {}` (`cloudflare_real_ip = true`) or in one site's `server {}`.

### 📡 API — `WebServerService`

| RPC | REST (CLI) | Purpose |
|---|---|---|
| `GetWebServer` | `GET /v1/webserver` | installed or not, mode, version, running or not, settings, last error, Cloudflare list date, whether nginx and Docker exist on the host |
| `InstallWebServerStream` | `POST /v1/webserver/install` `{mode}` | install or adopt; gRPC streams lines, REST answers `{log, webserver}` |
| `UninstallWebServer` | `DELETE /v1/webserver?purge=` | remove (`purge` also removes configs, sites and certificates) |
| `UpdateWebServerSettings` | `PUT /v1/webserver/settings` | new settings → `nginx -t` → apply |
| `TestWebServerConfig` / `ReloadWebServer` | `POST /v1/webserver/test` / `…/reload` | `nginx -t` without applying / a full apply |
| `GetWebServerFiles` | `GET /v1/webserver/files` | the resulting configuration files |
| `ListSites` | `GET /v1/webserver/sites` | sites with statuses |
| `ReplaceSites` / `UpsertSite` / `RemoveSite` | `PUT`/`DELETE /v1/webserver/sites/{id}` | sites desired state |
| `RenderSite` | — | render a site without applying it (UI preview) |
| `RenewCertificate` | `POST /v1/webserver/sites/{id}/renew` | order right away |
| `ProbeTcp` | — | a TCP connect from the node to an address (DMN-126) |

Capability in `GetStatus`: `webserver`.

### ⌨️ CLI

```
asc web status
asc web install [--mode system|docker]
asc web uninstall [--purge]
asc web test
asc web reload
asc web site list
asc web site add <name>... (--app <id> --port <port> | --to <host:port> [--to …]) [--balance round-robin|least-conn|ip-hash] [--health off|tcp|http] [--health-path /] [--id <id>] [--tls none|letsencrypt] [--cloudflare] [--no-websocket] [--max-body <size>]
asc web site show <id>
asc web site remove <id>
asc web site renew <id>
```

With no daemon running, the commands work in-process as root.

## 🔗 Related tasks

DMN-122, DMN-123, DMN-124, DMN-125, DMN-126 (health checks for the load balancer), DMN-067 (ACME for the daemon API) in [ROADMAP.md](../../../asc-platform/ROADMAP.md).
