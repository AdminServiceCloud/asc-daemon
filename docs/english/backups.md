# 💾 Backups (daemon)

## 📌 Description

The backup execution module on the node: creating, restoring and rotating backups of applications and databases. Policies and schedules are set by the platform ([💾 features/backups](../../../asc-platform/docs/features/backups.md)), but the module also works fully standalone via the CLI.

## 🎯 Scenarios

- `asc backup create <app>` — a full application backup (data + config + env manifest).
- `asc backup restore <app> <backup-id>` — restore.
- `asc backup list|prune` — listing and rotation.
- A task from the platform: a scheduled backup uploaded to S3.

## 🏗️ Technical design

- **Formats**: full (tar.gz + a metadata manifest) and incremental (based on a file snapshot).
- **What's included**: the application's volumes/directories, database dumps (via [🗄️ database](database.md)), the version manifest and env (secrets — encrypted only).
- **Storages**: a local directory, S3-compatible, SFTP, rsync — behind the `BackupStorage` trait.
- **Encryption**: optional AES-256-GCM before upload; the key comes from the platform or is local.
- **Rotation**: keep N latest / by age; runs after every successful backup.
- **Consistency**: pre/post backup hook commands from `asc.yaml` (e.g. `pg_dump` or pausing writes).
- **Integration**: schedules — via [⏰ scheduler](scheduler.md); platform-triggered runs — via taskmanager.

## 🔗 Related tasks

DMN-009, BE-005 in [ROADMAP.md](../../../asc-platform/ROADMAP.md).
