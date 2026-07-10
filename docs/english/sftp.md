# 📁 SFTP server (daemon)

## 📌 Description

A built-in SFTP server with per-application isolation: a user gets access to the files of a **specific application**, not the whole server. The foundation of the file manager in the platform UI and access for any SFTP client (FileZilla, WinSCP).

## 🎯 Scenarios

- 🖥️ The file manager in the platform UI (FE-008): browsing, uploading, editing application files.
- 🔧 A developer connects FileZilla to `sftp://node:2022` with credentials issued by the platform and sees only their application's directory.
- 🎮 A game designer uploads mods to a game server without OS access.

## 🏗️ Technical design

- **Server**: a built-in SSH/SFTP subsystem in Rust (russh), a separate port (2022 by default), no OS system users.
- **Isolation**: a virtual chroot onto the application directory (volumes for Docker, the working directory for native apps); path traversal is ruled out at the path-resolver level.
- **Accounts**: temporary credentials/keys are issued by the platform with a `user → application → permissions (ro/rw)` binding; TTL and revocation.
- **Permissions**: granting SFTP access is the `apps.files` permission ([🔐 access-control](../../../asc-platform/docs/features/access-control.md)).
- **Quotas and limits**: upload size and speed limits — per plan/config.
- **Audit**: a log of sessions and write operations.

## 🔗 Related tasks

DMN-010, FE-008 in [ROADMAP.md](../../../asc-platform/ROADMAP.md).
