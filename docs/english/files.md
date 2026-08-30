# 📁 Daemon file API

[🇷🇺 Русская версия](../russian/files.md)

## 📌 Description

`FileService` is part of the daemon API (see [📡 api](api.md)) and gives a view into and control over the node's filesystem from the root `/`: list a directory, stat a path, create a directory, move/rename, copy, delete, archive, and stream a file up or down. It is a separate service from `AppService` — it is not tied to any one application and does not replace the [📁 SFTP server](sftp.md), which gives an operator their own SFTP client chrooted to a single application's directory. `FileService` is the API behind the platform's file manager (the "Files" tab on a node's page).

## 🎯 Scenarios

- The platform lists a node's directory and shows it in the browser: `GET /v1/files?path=/etc`.
- An operator uploads a config from the Files tab: `PUT /v1/files/content?path=/etc/nginx/sites-available&name=app.conf`.
- An operator downloads a file: `GET /v1/files/content?path=/var/log/app.log`.
- An operator archives a few files before downloading: `POST /v1/files/archive`.
- CLI users and scripts use the same REST surface directly with a token, like the rest of the daemon API.

## 🏗️ Technical design

### Scope and access

The daemon runs as root, so `FileService` sees the whole filesystem — the platform is responsible for checking the calling user's permission (`files.read`/`files.edit`) before a request ever reaches the node. Because of that, **every method requires a root user context** and refuses otherwise:

- on the TCP transport (platform) the context is always full — no restriction there;
- on the CLI unix socket (`SO_PEERCRED`, see [📡 api](api.md)) the socket is intentionally world-connectable (`0666`), which used to be safe only because every operation went through an application's owner. `FileService` breaks that assumption, so it is **separately closed to a non-root peer** — an ordinary user gets `403`/`PERMISSION_DENIED` on every file route.

### Path resolution

A path must be absolute, contain no `..`, no NUL bytes, stay within sane length limits (4096 bytes per path, 255 bytes per component), and is lexically normalized. Resolution **deliberately skips `canonicalize`**: that resolves the final symlink, and the daemon's policy is "a symlink is shown, never walked through". An uploaded file's name is validated separately — no slash, no `..`, no NUL — since it comes from a browser and must never be able to choose a directory.

**Symlinks:** a directory entry is always reported with kind `SYMLINK`, never the kind of its target; alongside it comes the raw target (`symlink_target`) and, best-effort, the target's kind (`target_kind`, absent for a broken link). Recursive operations (delete, copy, archive) **never descend through** a symlink — the same discipline already used for app disk-usage walks and backups. Deleting a symlink unlinks the link itself, not the target; copying a symlink recreates the link rather than copying through it.

**Pseudo-roots `/proc`, `/sys`, `/dev`, `/run`:** listing works normally, but recursive operations (delete, copy, archive) refuse inside them — sizes there are fiction and a walk could hang forever.

**Protected paths** (`/`, `/boot`, `/etc`, `/usr`, `/var`, `/asc`) refuse as the exact target of a destructive operation — a guard rail against a mis-click, not a security boundary: root on the machine can still do the same thing by hand.

### Streaming

`ReadFile` is a server-stream of chunks; `WriteFile` is a client-stream of chunks with a header (`directory`, `name`, `overwrite`, mode) on the first message. Chunk size is **256 KiB**: comfortably under tonic's default 4 MiB decode limit, large enough to avoid drowning in per-syscall and per-HTTP/2-frame overhead.

An upload is written to a temporary file next to the target (`.asc-upload-<random>.part`, mode `0600`), fsynced, then renamed into place — the same atomic-write discipline used for the stored token. An interrupted upload never leaves a truncated file where a good one used to be — the temporary `.part` file simply stays behind and is cleaned up on the next attempt or a sweep. Without `overwrite`, a name collision is checked **before** the first byte is accepted.

### Archiving

Only `tar.gz` is supported (`tar` + `flate2`, already daemon dependencies). The `zip` format is reserved in the protocol and answers `UNIMPLEMENTED` — adding a zip encoder would complicate the aarch64/armv7 cross-build matrix with its own set of optional dependencies, and minimal dependencies is a project rule. An archive contains exactly the requested names, with no symlink members.

### Limits

Directory listing is capped at **10,000 entries**; past the cap, the response carries `truncated: true` and `total_entries`.

### Permissions and ownership

`SetPathAttributes` changes mode and/or owner/group — `chmod`/`chown` semantics: a field left unset stays untouched rather than resetting to a default. Owner and group are given by **name**, not a raw uid/gid — the caller is a UI dropdown, not a script — and the name is resolved to an id against `/etc/passwd`/`/etc/group`. Chowning a symlink follows it (plain `chown`, not `chown -h`) — the file manager operates on what a path resolves to.

`ListSystemIdentities` returns the machine's local users and groups — the source for that dropdown. It parses `/etc/passwd`/`/etc/group` directly rather than through `getpwent`/`getgrent`, which have no thread-safety story of their own and no need to be involved for a plain file read. Access is root-context-only like the rest of this service — not because the listing itself is sensitive (every file's owner/group name is already visible through `ListDirectory`), but because the service is not scoped per calling user at all.

### 🗺️ REST ↔ gRPC route map

| REST | gRPC | Description |
|---|---|---|
| `GET /v1/files?path=&hidden=` | `FileService.ListDirectory` | List a directory; `hidden=true` includes dotfiles |
| `GET /v1/files/stat?path=` | `FileService.StatPath` | Metadata for one path |
| `POST /v1/files/directory {"path","parents"}` | `FileService.CreateDirectory` | Create a directory |
| `POST /v1/files/move {"source","destination","overwrite"}` | `FileService.MovePath` | Move/rename |
| `POST /v1/files/copy {"source","destination","overwrite"}` | `FileService.CopyPath` | Copy |
| `POST /v1/files/delete {"paths":[...],"recursive"}` | `FileService.DeletePaths` | Delete; best-effort — one failure does not abort the rest |
| `POST /v1/files/archive {"directory","names","archive_path","format"}` | `FileService.CreateArchive` | Archive into `tar.gz` |
| `GET /v1/files/content?path=&offset=` | `FileService.ReadFile` (stream) | Download, resumable via `offset` |
| `PUT /v1/files/content?path=&name=&overwrite=` | `FileService.WriteFile` (stream) | Upload |
| `POST /v1/files/attributes {"path","mode"?,"owner"?,"group"?}` | `FileService.SetPathAttributes` | Change mode and/or owner/group (by name) |
| `GET /v1/files/identities` | `FileService.ListSystemIdentities` | The machine's local users and groups |

## 🔗 Related tasks

- DMN-070 — `FileService` implementation in the daemon.
- NODE-012 / BE-011 — nodeservice file RPCs and the platform REST facade that consume this API.
- FE-008 — the "Files" tab on the node page.
- [📁 sftp](sftp.md) — a neighboring but separate feature: an operator's own SFTP client, chrooted per application.
- [📁 file-manager](../../../asc-platform/docs/features/file-manager.md) — the platform-side overview of the feature.
