# ⏰ Scheduler (daemon)

## 📌 Description

The task scheduler inside the daemon: running operations on a schedule (cron syntax) without the platform. It runs two things — backups from app policies and its own job list (`asc schedule`): node reboot, app start/stop/restart/update, backup, shell command and HTTP request, with a run log. The platform puts its "run on the machine" schedules here — they fire even while the platform is unreachable.

## 🎯 Scenarios

- ⏰ A daily 03:00 backup to S3 keeping 7 copies — an app policy or a job: `asc schedule add daily@03:00 backup --app db --storage s3-main --keep 7`.
- 🌐 Every 5 minutes, check an address only reachable from the node: `asc schedule add "*/5 * * * *" http --url http://127.0.0.1:8080/health`.
- 🧩 A user cron task: `asc schedule add "0 5 * * *" shell --command ./cleanup.sh --app web` — in the app directory, as the app's owner.
- 🔁 A nightly app or node restart, an app update to the latest version.
- ☁️ A schedule created in the panel with "Run on the machine" turned on shows up here as a job with `managed_by = "platform"`.

## 🏗️ Technical design

Implemented in `src/daemon/scheduler/` (`mod.rs` — syntax and the loop, `jobs.rs` — jobs), started from the daemon's main loop.

### Evaluator

- Wakes up at the start of every minute; duplicate runs within one minute (fast loop, clock step) are deduplicated by wall-clock minute.
- Policy backups are one pass on the blocking pool, failures logged per app.
- Every due job starts in **its own** blocking task — a half-hour backup does not hold back an HTTP check due the same minute. A job whose previous run is still going skips the tick (no second instance on top of itself); manual runs over the API obey the same rule.

### Schedule syntax (`Schedule::parse`)

`hourly` (minute 0 of every hour — the platform's preset), `daily@HH:MM`, bare `HH:MM` (same as daily) or a five-field cron expression `minute hour day-of-month month day-of-week` with `*`, values, `a-b` ranges, `a,b,c` lists and `/n` steps; day-of-week 0–7 (0 and 7 are Sunday). Both date fields restricted follows the vixie-cron rule: either may match. Times are the node's local time; a job flagged `utc` is evaluated in UTC (how the platform pushes its schedules — its `next_run_at` is UTC too). `Schedule::next_after` computes the next firing (skipping hours and days that cannot match), `None` when there is none within a year.

### Jobs (`jobs.rs`)

| Action | What it does |
|---|---|
| `node_reboot` | `systemctl reboot` after 3 seconds — the run has time to be recorded |
| `app_start` / `app_stop` / `app_restart` | like `asc app start/stop/restart` |
| `app_update` | upgrade to the latest version; a running app is stopped for it and started again afterwards (also when the upgrade failed) |
| `backup` | back up to the given storages (empty — the app's policy, else `local`) with `keep` rotation; the archive is built once and pushed to each storage |
| `shell` | `/bin/sh -c`, stdout and stderr merged in write order; with `app` — in the app directory as the app's owner (`ASC_APP_ID`, `ASC_APP_DIR` in the environment), without — in `/` as the daemon's user; on timeout the whole process group is killed |
| `http` | GET/POST/PUT/PATCH/DELETE/HEAD with headers and a body; a response ≥ 400 fails the run |

- **Storage**: `<data_dir>/schedules.json` (0600 — an HTTP job may carry an `Authorization` header), the log — `<data_dir>/schedule-runs.json`: the last 50 runs per job, output and error capped at 64 KiB. Both files are rewritten whole via a temp file and `rename` under an exclusive `flock` on `schedules.lock`, so the CLI editing the list and the daemon recording a run never interleave.
- **Validation**: id `[A-Za-z0-9._-]{1,64}`, trigger syntax, non-empty app/command/URL (`http://` or `https://`), timeout 1–3600 s (default 600), `keep ≥ 1`.
- **Owner** (`managed_by`): operator jobs carry none; the platform writes its own as `managed_by = "platform"` through `ReplaceManagedSchedules` — a full replace of its own jobs only. A job owned by someone else cannot be replaced by an upsert or a push, and an id colliding with an operator job refuses the whole push (nothing is written).

### API (`ScheduleService`, capability `schedules`)

`ListSchedules` (with `next_run_unix` and the last run), `UpsertSchedule`, `RemoveSchedule`, `ReplaceManagedSchedules`, `RunSchedule` (run outside the trigger; answers with the opened run, status RUNNING, without waiting for it), `ListScheduleRuns`. REST mirror: `GET /v1/schedules`, `PUT/DELETE /v1/schedules/{id}`, `PUT /v1/schedules/managed/{managed_by}`, `POST /v1/schedules/{id}/run`, `GET /v1/schedules/{id}/runs` ([🔌 api](api.md)).

### CLI

`asc schedule list | add <trigger> <action> [flags] | remove | enable | disable | run | runs`. `run` executes the job in the foreground and prints its output; editing a platform job is allowed, but the CLI warns that the next sync overwrites it. The job files belong to root — on a system install run the commands with `sudo`.

### 📝 Next increments

- One-off delayed jobs; priorities and serializing mutually exclusive operations on one app; daemon auto-update waiting for an empty queue.

## 🔗 Related tasks

DMN-012, DMN-114, DMN-009, TASK-001, TASK-003, NODE-050, BE-005 in [ROADMAP.md](../../../asc-platform/ROADMAP.md).
