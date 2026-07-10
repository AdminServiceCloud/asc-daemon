# 🖥️ Consoles: WebSocket and SSH (daemon)

## 📌 Description

Two ways of interactive access from the platform to the node:

- 🔌 **WebSocket console** — a terminal and log stream of a specific application (attach to a container / a native application's journal) right in the browser.
- 🔐 **SSH console** — a full shell session on the server for the platform UI (developer mode).

## 🎯 Scenarios

- 🧑‍💻 A developer opens the application's "Console" tab — sees live output and types commands into the application's stdin (e.g. a Minecraft server console).
- 🔎 In the "Logs" tab they find the moment of a crash using timestamps and a filter.
- 🛠️ In developer mode they open the node's SSH terminal from the browser without a separate SSH client (available for servers added via an SSH key or machine credentials — see [🖧 nodeservice](../../../asc-platform/docs/services/nodeservice.md)).
- 🤖 The AI assistant uses the same execution layer via `exec_command`.

## 🏗️ Technical design

### 🖥️ Application tabs in the UI

Every application in the platform has two tabs:

- **Console** — real time: a stream of the Docker application's output or the native application's runtime process; **at the bottom — an input line for the application's stdin**. This is an interactive session (attach), not just viewing.
- **Logs** — history: **every line carries a timestamp**, text search/filtering, sorting by time, range selection; a "show in context" jump from a search result.

### 🎨 Terminal

The terminal is modern and convenient: xterm.js, **full ANSI color support** and control sequences, automatic sizing (resize → PTY), copy/paste, buffer search, scrollback history, font/theme selection (inherits the UI theme).

### ⚙️ Transport

- **WebSocket console**: a server inside the daemon; sessions `logs` (a read-only stream: docker logs -f / journald follow, with timestamps) and `attach` (the application's PTY/stdin, bidirectional). The protocol is binary frames (stdin/stdout/resize) as in ttyd/gotty.
- **SSH console**: a PTY shell session as the configured user; the transport to the browser is the same WebSocket channel through the nodeservice tunnel (no SSH port is exposed). For servers added via an SSH key/machine credentials, the SSH console is available even before the daemon is installed.
- **Access via a temporary token**: a WebSocket connection to the console opens **only with a temporary token**, issued **through the daemon API** (`AppService.IssueConsoleToken` / `POST /v1/apps/{id}/console-token`): TTL 30 seconds, single-use, bound to the application and the session type `logs`/`attach`. The platform obtains the token automatically when the console tab opens (the backend checks permissions → requests a token from the daemon → hands it to the browser); in standalone mode the CLI issues it (`asc app console-token <id>`).
- **Endpoint**: `GET /v1/console?token=<token>[&tail=N]` — outside the API's bearer authentication (a browser cannot set headers on a WS handshake); the only protection is the single-use token; an invalid/used token → 401 before the upgrade.
- **MVP implementation (DMN-007)**: the `logs` session — text frames, source by runtime: for Docker — **log streaming via the Engine API** (`follow` + timestamps over the unix socket); for systemd — `journalctl -f -o short-iso`; for process — `tail -F app.log` (the subprocess is killed when the client disconnects). The `attach` session — Docker only for now (**Engine API `attach`**, binary stdin/stdout frames); systemd/process need a PTY at launch — that arrives with the ttyd-style binary protocol (resize etc.) during the UI work (FE-006).
- **Multiple connections to one application** (tabs/users): `attach` sessions are multi-client following the wings pattern — the daemon creates **one shared source session** (hub) per application and its output is fanned out to all connections via a broadcast channel; a lagging client loses old chunks but does not slow down the rest. The hub keeps a **replay buffer** of the last ~128 KiB of output — a new tab immediately sees recent output, and all clients' stdin converges into the container's single pipe. The source (Engine API `attach`) closes together with the last client. `logs` sessions are independent by construction: every connection is its own follow stream with its own `tail`, so multiple tabs work there too.
- **CLI**: `asc attach <id>` (synonym `asc app attach <id>`) — an interactive application console right from the server terminal: the CLI process's stdin/stdout are piped into the application. Works standalone (without the daemon running) — the CLI, like the other `asc app` commands, goes straight to the Engine API; Docker itself fans the output out to everyone attached (CLI + browser tabs). Docker applications only for now; systemd/process — together with PTY (FE-006). Disconnect — Ctrl+C, the application keeps running.
- **Routing**: browser ↔ nodeservice (wss) ↔ daemon tunnel ↔ console module.
- **Permissions**: `apps.console` for the application console, `nodes.ssh` for SSH ([🔐 access-control](../../../asc-platform/docs/features/access-control.md)); SSH is available only in developer mode ([🎛️ ui-modes](../../../asc-platform/docs/features/ui-modes.md)).
- **Audit**: recording of SSH session commands (an opt-in organization policy).

## 🔗 Related tasks

DMN-007, DMN-008, FE-006 in [ROADMAP.md](../../../asc-platform/ROADMAP.md).
