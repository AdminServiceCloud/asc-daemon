# 💾 Backups (daemon)

## 📌 Description

The backup execution module on the node: creating, restoring and rotating backups of applications. Policies and schedules can be set by the platform ([💾 features/backups](../../../asc-platform/docs/features/backups.md)), but the module also works fully standalone via the CLI (`asc backup ...`) — no platform account required.

## 🎯 Scenarios

- `asc backup create <app>` — back up an app to its configured storages (`local` by default), or a specific one with `--storage <name>` (repeatable).
- `asc backup restore <app> <backup-id>` — restore; the app must be stopped first (destructive: replaces its repository, config and data).
- `asc backup list <app> [--storage <name>]` — an app's backups on one storage, oldest first.
- `asc backup prune <app> --keep <n> [--storage <name>]` — delete the oldest backups beyond `n` by hand (rotation also runs automatically after `create`, from the app's own `keep` setting).
- `asc backup storage add|list|remove` — manage where backups go; `asc app settings <app>` (category `backups`) — which of those storages this app backs up to, how many copies to keep, and how often (the schedule runs inside the daemon, see [⏰ scheduler](scheduler.md)).

## 🏗️ Technical design

### What's backed up

An archive (`tar.gz`) of the app directory's `repository/`, `config/` and `data/` subdirectories — everything except `meta.json` (regenerated on restore, like a [🧬 clone](app-management.md)). `asc.backup.yaml` at the package repository root excludes paths from the archive:

```yaml
exclude:
  - data/cache/**
  - repository/vendor
```

Patterns are relative to the app directory and support `*` (any run of characters within one path segment), `**` (any run, crossing `/`) and `?` (one character); excluding a directory excludes everything under it, like `.gitignore`. No file is a substitute for application-level consistency (e.g. a database dump) — pre/post backup hooks are a later increment.

### Storages (`BackupStorage` trait, `src/daemon/backup/storage.rs`)

- **`local`** — always available, no setup: a plain directory (`<data_dir>/backups`, i.e. `/var/lib/asc/backups` by default). This is the only storage kind that actually transfers anything today.
- **`s3`** — an S3-compatible storage (AWS S3, MinIO, Backblaze B2, Wasabi, Yandex Object Storage…), DMN-115: `asc backup storage add <name> --type s3 --bucket … --region … [--endpoint …] --access-key … --secret-key … [--prefix …]`. A real transfer (`src/daemon/backup/s3.rs`): Signature V4 signed by hand (HMAC-SHA256 over the `sha2` crate already in the tree) and the blocking `ureq` HTTPS client on the same rustls/ring stack the daemon API uses. Archives up to 64 MiB go up in one `PUT`, larger ones as multipart (16 MiB+ parts; a failed upload is aborted); upload bodies are sent as `UNSIGNED-PAYLOAD` (the archive is not read twice), every other request signs its body hash. Addressing is virtual-hosted for AWS and path-style elsewhere (the same choice the platform's connection check makes); listing uses paginated `ListObjectsV2` and returns only archives of exactly that app (`demo` does not pick up `demo-2`). Errors carry S3's own code (`SignatureDoesNotMatch`, `NoSuchBucket`…), never the credentials.
- **`ftp` / `sftp`** — configured the same way (connection details persist like registry sources — a system list `/etc/asc/backup-storages.toml`, root-managed and visible to everyone, plus a user list `~/.config/asc/backup-storages.toml`; the file is 0600, since it holds credentials), but the transfer is not implemented yet — every operation returns a clear "not implemented" error.
- `managed_by` on a storage entry: the platform stores its organization S3 storages as `platform-<id>` with `managed_by = "platform"` (key prefix `<path_prefix>/asc-backups/<node id>`) and may update them; it never replaces an operator's entry, nor the reverse.
- A configured storage's name cannot be `local` (reserved) and a regular user cannot shadow or remove a system-scoped storage, same rules as [📦 registry sources](package-manager.md).

### Backup policy (`asc app settings` → `backups`)

Stored under the `$backup` reserved key in `config/settings.json`, alongside `$quota`/`$start_command` (same convention, DMN-017/030): `storages` (multi-select, toggled by number in the editor), `keep` (copies to retain per storage — pruned automatically right after each `create`), `schedule` (`daily@HH:MM`, bare `HH:MM`, or a five-field cron expression `min hour day month weekday`; validated by the editor). **`schedule` is enforced by the daemon's scheduler** ([⏰ scheduler](scheduler.md), DMN-012): once a minute it evaluates every app's policy against the node's local time and runs the due backups to the policy's storages with the policy's `keep` rotation — the daemon must be running (`asc service install` or `asc serve`). `asc backup create <app>` without `--storage` uses the policy's storages, falling back to `local` alone when the policy is empty.

### Listing and API (`BackupService`, capability `backups`)

A listing reports each archive's name, storage, size and creation time (from the `<app>-<unix-ts>.tar.gz` name); `asc backup list` prints name and size. A backup to several storages builds the archive **once** and pushes the same file to each — every copy is one snapshot; the result comes back per storage. Archive names for restore and delete are validated (`validate_backup_name`): a path with `/` or another app's archive is refused before the name reaches a path or an object key.

API: `ListBackupStorages`, `UpsertBackupStorage` (S3 or a local directory, with `managed_by`), `RemoveBackupStorage`, `ListBackups` (one app or all, one storage or all; an unreachable storage lands in `errors` instead of failing the call), `CreateBackup`, `RestoreBackup` (`stop_app` — stop, restore and start again), `DeleteBackup`. REST mirror: `GET /v1/backups`, `GET /v1/backups/storages`, `PUT/DELETE /v1/backups/storages/{name}`, `POST /v1/apps/{id}/backups`, `POST /v1/apps/{id}/backups/restore`, `DELETE /v1/apps/{id}/backups/{storage}/{name}` ([🔌 api](api.md)). The platform builds on it ([💾 features/backups](../../../asc-platform/docs/features/backups.md)).

### Restore

Downloads the archive to a local temp file, then **replaces** the app directory's `repository/`, `config/` and `data/` wholesale (removed, then extracted) — the result is exactly the backed-up snapshot, not a merge with whatever was there. The CLI refuses to restore over a running app.

## 🔗 Related tasks

DMN-009, DMN-012, DMN-115, NODE-049, BE-005 in [ROADMAP.md](../../../asc-platform/ROADMAP.md).
