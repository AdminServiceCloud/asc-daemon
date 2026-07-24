# 📊 Monitoring (daemon)

## 📌 Description

Collecting system and application metrics on the node: CPU, RAM, disk, network, Docker and systemd state. Metrics are available through the CLI and the daemon API, and are streamed to the platform for the dashboard and alerts.

## 🎯 Scenarios

- `asc status` — a server and application summary in the terminal.
- `asc stats` — CPU, memory, disk usage and I/O, and network I/O per application (like `docker stats --no-stream`); `asc stats --live` keeps refreshing in place instead of exiting after one sample (like plain `docker stats`); root sees all users' applications grouped by owner.
- The platform dashboard shows node load graphs in real time.
- An "application went down" alert → a Telegram notification (via the platform).
- The AI assistant answers "what's eating my memory?" with monitoring data.

## 🏗️ Technical design

- **System metrics**: CPU, memory, disk (usage + I/O), network (rx/tx, errors, drops) — directly from procfs (`/proc/stat`, `/proc/meminfo`, `/proc/loadavg`, `/proc/net/dev`, `/proc/uptime`) and `statvfs(3)` for disks; no external crates. Nothing distro-specific here; a separate collector behind the same abstraction will come later for macOS.
- **Application metrics**: per-container stats (Docker API), per-unit (systemd cgroups), per-process.
- **Network interfaces**: an interface list (IP, MAC, status, speed, type), traffic statistics; interface management is post-MVP.
- **Storage**: a ring buffer in memory + a short history in SQLite; long history lives on the platform side.
- **Delivery**: a push stream into the nodeservice tunnel (5–15 s interval, adaptive); healthcheck events — instantly.
- **Application health statuses**: `running / stopped / unhealthy / unknown` — based on the healthcheck from `asc.yaml`.

### 🧩 Implementation (current increment)

- The `src/daemon/monitor/` module: `system.rs` — procfs parsers (pure functions over `&str`, covered by unit tests) and snapshot capture; `mod.rs` — `Monitor`: a background sampler in the daemon; the interval and history depth are set in `config.toml` (`[monitor] interval_secs = 10`, `history_samples = 360` — an hour of history at 10 s).
- CPU usage is computed as the delta of two `/proc/stat` reads; network rates (bytes/s) — as counter deltas between samples.
- **API**: `MonitorService` in proto (`GetSystemMetrics`, `GetMetricsHistory`) + REST routes `GET /v1/metrics` and `GET /v1/metrics/history?limit=N` — both transports on top of the shared layer, like the rest of the API (DMN-005).
- **CLI**: `asc status` shows CPU (usage, load average), memory and disks — the metrics are sampled by the CLI itself without contacting the daemon (autonomy).
- **Per-app metrics**: the `usage()` method on the `AppDriver` trait returns cumulative counters (CPU time in microseconds, resident memory in bytes, block-device read/write bytes, network rx/tx bytes — the I/O counters are `Option`, `None` when the runtime cannot report them); sources by runtime — the Docker Engine API (`/containers/<id>/stats`, one-shot: `blkio_stats.io_service_bytes_recursive` for disk I/O, `networks` for network I/O, both real because a container has its own cgroup and network namespace), the systemd unit's cgroup v2 (`cpu.stat` + `memory.current` + `io.stat` for disk I/O; network I/O is always `None` — a unit shares the host's network namespace, so there is nothing per-unit to read), `/proc/<pid>/stat` + `statm` + `io` for processes (disk I/O from `read_bytes`/`write_bytes`; network I/O always `None` for the same reason as systemd). CPU% is computed as the delta of two samples (~500 ms), like `docker stats`, and can exceed 100% on multi-core machines.
- **CLI `asc stats`**: an ID / KIND / CPU % / MEM / QUOTA / DISK / NET I/O / DISK I/O table over running applications — QUOTA is a compact usage bar (like `asc app disk`) when the app has a disk quota set, else a dash, DISK is the app directory's total size regardless; NET I/O and DISK I/O are "rx / tx" and "read / write" cumulative byte totals since the app started (like `docker stats`'s own NET I/O / BLOCK I/O columns — not a rate), a dash when the runtime cannot report the pair (network I/O is only available for Docker apps, see above); sorting `--sort cpu|mem` (cpu by default); for root — grouping by owner, like `asc app list`. Stopped applications are shown with dashes. `--live` reprints the same table in place (screen clear + redraw) every ~500 ms — the same interval a single sample already costs for the CPU delta, so no extra sleep — until Ctrl+C.
- Next increments: per-app metrics in the API (`MonitorService`), history in SQLite, healthcheck statuses, pushing into the tunnel.

## 🔗 Related tasks

DMN-006, NODE-003, FE-004 in [ROADMAP.md](../../../asc-platform/ROADMAP.md).
