# ⏰ Scheduler (daemon)

## 📌 Description

The task scheduler inside the daemon: running operations on a schedule (cron syntax) and managing the node's background task queue. Used by backups, certificate renewal, auto-updates and user-defined tasks.

## 🎯 Scenarios

- ⏰ A daily backup at 03:00 (a policy from the platform).
- 🔁 A daily check of Let's Encrypt certificate expiry.
- 🔄 A daemon auto-update — postponed while there are active tasks; `--force` to run anyway.
- 🧩 A user cron task: `asc task add "0 5 * * *" -- ./cleanup.sh`.

## 🏗️ Technical design

### ✅ First increment (shipped): schedule evaluator + backups

Implemented in `src/daemon/scheduler/` and started from the daemon's main loop:

- **Evaluator**: wakes up at the start of every minute and runs whatever is due; a pass runs on the blocking pool and failures are logged per app — one broken app never stops the rest. Duplicate runs within one minute (fast loop, clock step) are deduplicated by wall-clock minute.
- **Schedule syntax** (`Schedule::parse`): `daily@HH:MM`, bare `HH:MM` (same as daily) or a five-field cron expression `minute hour day-of-month month day-of-week` with `*`, values, `a-b` ranges, `a,b,c` lists and `/n` steps; day-of-week 0–7 (0 and 7 are Sunday). Both date fields restricted follows the vixie-cron rule: either may match. Times are the node's local time.
- **Consumer — scheduled backups (DMN-009)**: every app whose backup policy (`asc app settings`, category `backups`) has a `schedule` gets backups created to the policy's storages with its `keep` rotation. The settings editor validates the schedule syntax on input.

### 📝 Next increments (planned)

- **Task types**: Install, Update, Uninstall, Backup, Restore, CertRenew, Custom.
- **States**: `Pending → Running → Completed | Failed | Cancelled`; priorities Low/Normal/High/Critical.
- **Queue**: prioritized, persistent (SQLite) — survives a daemon restart; unfinished tasks are correctly resumed or marked Failed.
- **Schedules**: one-off delayed tasks in addition to cron.
- **Concurrency**: mutually exclusive tasks (two operations on one application) are serialized; auto-update waits for an empty queue.
- **CLI**: `asc task list|add|cancel|info`; the API — for the platform's taskmanager (task statuses are streamed upward).

## 🔗 Related tasks

DMN-012, TASK-001, BE-005, BE-006 in [ROADMAP.md](../../../asc-platform/ROADMAP.md).
