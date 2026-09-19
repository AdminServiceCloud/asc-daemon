# 👤 Local user management

[🇷🇺 Русская версия](../russian/user-management.md)

## 📌 Description

`UserService` is part of the daemon API (see [📡 api](api.md)) and gives control over the node's **local Linux accounts**: list every account (including root), create and delete accounts, lock/unlock, change the login shell, manage supplementary groups (`sudo`, `docker`, ...), and deploy an organization's SSH public key into an account's `~/.ssh/authorized_keys`. It is the API behind the platform's "Users" tab on a node's detail page. Unlike [📁 FileService](files.md), `UserService` has no `app_id` scoping — this is whole-machine account administration, not a per-app feature.

The daemon never handles a Linux password: this feature is **SSH-key-only by design**. `CreateUser` never passes `-p` to `useradd`, so a freshly created account starts with `useradd`'s own no-password default (a `!` hash in `/etc/shadow`) — login only becomes possible once a key is deployed with `AddAuthorizedKey`.

## 🎯 Scenarios

- The platform lists a node's local accounts for the "Users" tab: `GET /v1/users`.
- An operator provisions a new deploy account with `sudo` and `docker` access: `POST /v1/users {"name":"deploy","groups":["sudo","docker"]}`.
- An operator suspends an account without deleting it (e.g. an employee's offboarding): `PUT /v1/users/{name}/locked {"locked":true}`.
- An organization pushes a team member's public key onto a shared account: `POST /v1/users/{name}/authorized-keys {"public_key":"ssh-ed25519 AAAA... alice@laptop"}` — calling it again with the same key is a no-op, not a duplicate line.
- An operator revokes one key without touching the others: `DELETE /v1/users/{name}/authorized-keys {"fingerprint":"SHA256:..."}`.

## 🏗️ Technical design

### Scope and access

Like `FileService`, `UserService` requires a root caller [`UserContext`](../../src/daemon/apps/mod.rs) for **every** method (`users::require_root`): the TCP transport (platform) always presents a full-rights context, and the CLI unix socket — otherwise world-connectable — is separately closed to a non-root peer. This is whole-machine account administration, not scoped per calling user at all, so there is no `app_id`-style narrowing here.

### Listing and parsing

`ListUsers` parses `/etc/passwd`, `/etc/shadow` and `/etc/group` directly, independently of `FileService`'s own (narrower) `/etc/passwd`/`/etc/group` parse used for the file manager's ownership dropdown — a bug in one must never affect the other. A account's `locked` flag comes from `/etc/shadow`'s password-hash field starting with `!` (including the `!!` variant some tools use); `groups` lists only **supplementary** memberships resolved from `/etc/group`'s member lists — a group that happens to equal the account's own primary gid is excluded even if the account is also listed there explicitly. `is_system` is `uid < 1000` — root (uid 0) is included in the listing and flagged as a system account, not hidden. Results are sorted by uid ascending.

### Creating and deleting accounts

`CreateUser` shells out to `useradd` rather than hand-editing `/etc/passwd`: distro-correct handling of `/etc/shadow`, skeleton files (`/etc/skel`) and NSS is exactly what that tooling is for. Every requested supplementary group is checked against `/etc/group` **before** `useradd` ever runs, so an unknown group fails with the daemon's own typed error instead of a `useradd` stderr message. `home`, when given, must be an absolute path; `create_home` defaults to `true` (`useradd -m`, `-M` otherwise).

`DeleteUser` shells out to `userdel` (`-r` for `remove_home`) and **hard-refuses uid 0 and any uid below 1000** — a fixed safety rail with no override, since this deletes a Linux account, not a platform record, and system/service accounts are not this feature's business.

### Lock/unlock, shell and groups

`SetUserLocked` runs `usermod -L`/`-U`. On most distros with `UsePAM yes` in `sshd_config`, a locked account is refused by `pam_unix`'s account-management check even for an otherwise-successful pubkey authentication — so this is a real "suspend access" control for a key-only account, not a cosmetic flag; the exact behavior still depends on the distro's PAM configuration.

`SetUserShell` requires the shell to be an absolute path already listed in `/etc/shells` — the same allowlist `chsh` itself enforces — checked before `usermod -s` ever runs.

`SetUserGroups` **replaces** the full supplementary group set, `usermod -G`-style, rather than exposing an add/remove API: a partial API would let two operators changing groups concurrently silently clobber each other's change. An empty `groups` list clears every supplementary group (`usermod -G ''`, not the flag omitted). Every requested group is checked against `/etc/group` before `usermod` runs, same as `CreateUser`.

### Authorized keys

An account's `~/.ssh/authorized_keys` path is always derived from its **resolved** home directory (looked up fresh via the account list), never from a client-supplied path.

`ListAuthorizedKeys` returns an empty list rather than an error when the file does not exist yet — a fresh account simply has none. Parsing only recognizes lines whose first token is a plain key-type prefix (`ssh-`, `ecdsa-`, `sk-` — the overwhelming majority of machine-managed entries); a line that instead opens with an options string (e.g. `command="...",... ssh-ed25519 ...`) is left alone rather than parsed, and a line that fails to fingerprint (hand-edited garbage) is skipped rather than failing the whole call. Each key's fingerprint (`SHA256:...`) comes from `ssh-keygen -lf -`.

`AddAuthorizedKey` is **idempotent**: adding a key whose fingerprint already exists in the file returns that existing entry unchanged instead of duplicating the line. Otherwise it creates `~/.ssh` (`0700`) and the file (`0600`) if needed, chowns both to the account's uid/gid, and appends the trimmed key line.

`RemoveAuthorizedKey` treats a missing file as already-removed (`Ok`, no error) — matching the idempotent spirit of `AddAuthorizedKey`. It rewrites the file atomically (write to a `.tmp` file in the same directory, preserve the original file's mode, then rename over the original) and never touches ownership on rewrite. A line that cannot be parsed as a key (an options-string line, hand-edited garbage) is always kept untouched, whether or not it happens to match by coincidence.

### 🗺️ REST ↔ gRPC route map

| REST | gRPC | Description |
|---|---|---|
| `GET /v1/users` | `UserService.ListUsers` | Every local account, root included, ordered by uid ascending |
| `POST /v1/users {"name","home"?,"shell"?,"create_home"?,"groups"?}` | `UserService.CreateUser` | Create an account via `useradd` |
| `DELETE /v1/users/{name}?remove_home=` | `UserService.DeleteUser` | Delete via `userdel`; refuses uid 0 and any uid below 1000 |
| `PUT /v1/users/{name}/locked {"locked"}` | `UserService.SetUserLocked` | Lock/unlock via `usermod -L`/`-U` |
| `PUT /v1/users/{name}/shell {"shell"}` | `UserService.SetUserShell` | Change login shell; must be listed in `/etc/shells` |
| `PUT /v1/users/{name}/groups {"groups":[...]}` | `UserService.SetUserGroups` | Replace the full supplementary group set |
| `GET /v1/users/{name}/authorized-keys` | `UserService.ListAuthorizedKeys` | Parsed `authorized_keys` entries; options-string lines are skipped |
| `POST /v1/users/{name}/authorized-keys {"public_key"}` | `UserService.AddAuthorizedKey` | Idempotent append |
| `DELETE /v1/users/{name}/authorized-keys {"fingerprint"}` | `UserService.RemoveAuthorizedKey` | Remove by fingerprint (body, not path, to sidestep URL-encoding `/`) |

## 🔗 Related tasks

- DMN-100 — `UserService` implementation in the daemon.
- The `users` capability flag (see `ApiState::CAPABILITIES`) gates the platform's "Users" tab client-side, the same pattern as `sources`/`credentials`.
- [📁 files](files.md) — the neighboring, node-wide file API; `UserService` follows the same root-gating discipline but has no `app_id` scoping.
