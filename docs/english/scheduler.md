# ⏰ Scheduler (daemon)

## 📌 Description

The task scheduler inside the daemon: running operations on a schedule (cron syntax) and managing the node's background task queue. Used by backups, certificate renewal, auto-updates and user-defined tasks.

## 🎯 Scenarios

- ⏰ A daily backup at 03:00 (a policy from the platform).
- 🔁 A daily check of Let's Encrypt certificate expiry.
- 🔄 A daemon auto-update — postponed while there are active tasks; `--force` to run anyway.
- 🧩 A user cron task: `asc task add "0 5 * * *" -- ./cleanup.sh`.

## 🏗️ Technical design

- **Task types**: Install, Update, Uninstall, Backup, Restore, CertRenew, Custom.
- **States**: `Pending → Running → Completed | Failed | Cancelled`; priorities Low/Normal/High/Critical.
- **Queue**: prioritized, persistent (SQLite) — survives a daemon restart; unfinished tasks are correctly resumed or marked Failed.
- **Schedules**: cron expressions + one-off delayed tasks; the node's timezone.
- **Concurrency**: mutually exclusive tasks (two operations on one application) are serialized; auto-update waits for an empty queue.
- **CLI**: `asc task list|add|cancel|info`; the API — for the platform's taskmanager (task statuses are streamed upward).

## 🔗 Related tasks

DMN-012, TASK-001, BE-005, BE-006 in [ROADMAP.md](../../../asc-platform/ROADMAP.md).
