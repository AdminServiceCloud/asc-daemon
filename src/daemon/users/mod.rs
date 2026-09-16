//! Local Linux account management (DMN-099): list every local account
//! (including root), create/delete accounts, lock/unlock, change shell,
//! manage supplementary groups (sudo, docker, ...), and deploy an SSH
//! public key into an account's `~/.ssh/authorized_keys`. See
//! docs/user-management.md.
//!
//! Like `files`, the daemon runs as root and this service sees the whole
//! machine's accounts — the platform performs its own per-user
//! authorization before a request ever reaches here. Because of that,
//! every entry point requires a root caller [`UserContext`] (see
//! [`require_root`]): the unix socket is otherwise world-connectable and
//! authorizes purely by peer uid, a rule this service must not inherit.
//!
//! Parsing here is intentionally self-contained (`/etc/passwd`,
//! `/etc/shadow`, `/etc/group` are each read fresh, independently of
//! `files::parse_passwd`/`files::parse_group`): a bug in this richer,
//! account-management-flavored parse must never affect the file manager's
//! ownership dropdown, and vice versa.

mod authorized_keys;

use crate::daemon::apps::UserContext;

pub use authorized_keys::{
    AuthorizedKey, add_authorized_key, list_authorized_keys, remove_authorized_key,
};

/// Typed account-management error, downcastable by the gRPC/REST
/// transports so e.g. "already exists" reaches the caller as such instead
/// of collapsing into a generic internal error (mirrors `files::FileError`).
#[derive(Debug)]
pub enum UserError {
    NotFound(String),
    AlreadyExists(String),
    /// A caller without a root context, or a hard safety rail (e.g.
    /// deleting a system account) that has no override.
    Protected(String),
    InvalidInput(String),
    /// A requested supplementary group has no `/etc/group` entry.
    UnknownGroup(String),
    CommandFailed {
        command: String,
        stderr: String,
    },
    Io(std::io::Error),
}

impl std::fmt::Display for UserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UserError::NotFound(name) => write!(f, "not found: {name}"),
            UserError::AlreadyExists(name) => write!(f, "already exists: {name}"),
            UserError::Protected(msg) => write!(f, "protected: {msg}"),
            UserError::InvalidInput(msg) => write!(f, "invalid input: {msg}"),
            UserError::UnknownGroup(name) => write!(f, "unknown group: {name}"),
            UserError::CommandFailed { command, stderr } => {
                write!(f, "{command} failed: {stderr}")
            }
            UserError::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for UserError {}

pub type Result<T> = std::result::Result<T, UserError>;

/// Refuse anything other than a full-visibility caller — this is
/// whole-machine account administration, not scoped per calling user at
/// all. Mirrors `files::require_root`.
pub fn require_root(ctx: &UserContext) -> Result<()> {
    if ctx.is_root {
        Ok(())
    } else {
        Err(UserError::Protected("root is required".into()))
    }
}

/// One local Linux account, uid 0 (root) included.
#[derive(Debug, Clone)]
pub struct ManagedUser {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: String,
    pub shell: String,
    /// `/etc/shadow`'s password-hash field starts with `!` (including
    /// `!!`, the useradd/passwd-locked convention).
    pub locked: bool,
    /// `uid < 1000` — surfaced for visibility, not hidden, but blocked
    /// from [`delete_user`].
    pub is_system: bool,
    /// Supplementary groups only, sorted by name — never the account's own
    /// primary group.
    pub groups: Vec<String>,
}

/// One `/etc/group` line, fully parsed (unlike `files::SystemGroup`, which
/// only needs name/gid): membership drives [`read_accounts`]'s
/// supplementary-group resolution.
struct GroupEntry {
    name: String,
    gid: u32,
    members: Vec<String>,
}

/// Parses `/etc/group` directly, the same technique `files::parse_group`
/// uses for its own, narrower purpose — kept independent on purpose (see
/// the module doc comment).
fn parse_group_entries() -> Result<Vec<GroupEntry>> {
    let raw = std::fs::read_to_string("/etc/group").map_err(UserError::Io)?;
    let mut groups = Vec::new();
    for line in raw.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // name:password:gid:member1,member2,...
        let mut fields = line.split(':');
        let Some(name) = fields.next() else { continue };
        fields.next(); // password placeholder
        let Some(gid) = fields.next().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let members = fields
            .next()
            .unwrap_or("")
            .split(',')
            .filter(|m| !m.is_empty())
            .map(|m| m.to_string())
            .collect();
        groups.push(GroupEntry {
            name: name.to_string(),
            gid,
            members,
        });
    }
    Ok(groups)
}

/// Every local group's name/gid — used to validate a requested
/// supplementary group exists, before ever shelling out to
/// `useradd`/`usermod` (see [`create_user`], [`set_user_groups`]).
fn read_groups() -> Result<Vec<(String, u32)>> {
    Ok(parse_group_entries()?
        .into_iter()
        .map(|g| (g.name, g.gid))
        .collect())
}

/// `/etc/shadow`'s locked flag per account name. A missing entry (should
/// not happen for a real account) reads as not locked rather than erroring
/// the whole listing.
///
/// An unreadable `/etc/shadow` itself — permission denied, or the file does
/// not exist at all (some minimal container images ship without one) —
/// reads as "every account unlocked" rather than failing [`list_users`]
/// outright: lock state is one field of many here, and in the one place
/// this service is actually called from (behind [`require_root`]) the
/// daemon's own euid can always read it, so this only ever softens an
/// unusual environment instead of hiding a real caller-facing permission
/// problem.
fn read_locked_flags() -> Result<std::collections::HashMap<String, bool>> {
    let raw = match std::fs::read_to_string("/etc/shadow") {
        Ok(raw) => raw,
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
            ) =>
        {
            return Ok(std::collections::HashMap::new());
        }
        Err(err) => return Err(UserError::Io(err)),
    };
    let mut locked = std::collections::HashMap::new();
    for line in raw.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split(':');
        let Some(name) = fields.next() else { continue };
        let hash = fields.next().unwrap_or("");
        locked.insert(name.to_string(), hash.starts_with('!'));
    }
    Ok(locked)
}

/// Parses `/etc/passwd`, `/etc/shadow` and `/etc/group` into the richer
/// [`ManagedUser`] view this service needs — name, uid, gid, home, shell,
/// lock state and supplementary groups. Sorted by uid ascending.
fn read_accounts() -> Result<Vec<ManagedUser>> {
    let raw = std::fs::read_to_string("/etc/passwd").map_err(UserError::Io)?;
    let locked_flags = read_locked_flags()?;
    let groups = parse_group_entries()?;

    let mut accounts = Vec::new();
    for line in raw.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // name:password:uid:gid:gecos:home:shell
        let mut fields = line.split(':');
        let Some(name) = fields.next() else { continue };
        fields.next(); // password placeholder
        let Some(uid) = fields.next().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Some(gid) = fields.next().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        fields.next(); // gecos
        let home = fields.next().unwrap_or_default().to_string();
        let shell = fields.next().unwrap_or_default().to_string();

        // Supplementary only: a group whose gid equals the account's own
        // primary gid is excluded even if the account is also listed
        // explicitly among that group's members in /etc/group.
        let mut member_groups: Vec<String> = groups
            .iter()
            .filter(|g| g.gid != gid && g.members.iter().any(|m| m == name))
            .map(|g| g.name.clone())
            .collect();
        member_groups.sort();

        accounts.push(ManagedUser {
            name: name.to_string(),
            uid,
            gid,
            home,
            shell,
            locked: locked_flags.get(name).copied().unwrap_or(false),
            is_system: uid < 1000,
            groups: member_groups,
        });
    }
    accounts.sort_by_key(|u| u.uid);
    Ok(accounts)
}

/// Every local account, root included, ordered by uid ascending.
pub fn list_users() -> Result<Vec<ManagedUser>> {
    read_accounts()
}

/// POSIX portable username rule: `^[a-z_][a-z0-9_-]*$`, at most 32 bytes.
/// Called first by every function below that takes a `name`/`user`
/// argument — defense in depth: `std::process::Command` arguments are
/// never shell-interpreted, but a malformed name should fail with a clear
/// typed error instead of a confusing `useradd`/`usermod` stderr.
fn validate_username(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 32 {
        return Err(UserError::InvalidInput(format!("invalid username: {name}")));
    }
    let mut chars = name.chars();
    let first_ok = matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c == '_');
    let rest_ok =
        chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if !first_ok || !rest_ok {
        return Err(UserError::InvalidInput(format!("invalid username: {name}")));
    }
    Ok(())
}

fn run_command(command: &str, cmd: &mut std::process::Command) -> Result<()> {
    let output = cmd.output().map_err(UserError::Io)?;
    if !output.status.success() {
        return Err(UserError::CommandFailed {
            command: command.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(())
}

fn find_account(name: &str) -> Result<ManagedUser> {
    read_accounts()?
        .into_iter()
        .find(|u| u.name == name)
        .ok_or_else(|| UserError::NotFound(name.to_string()))
}

/// Creates a Linux account via `useradd` rather than hand-editing
/// `/etc/passwd`: distro-correct handling of `/etc/shadow`, skeleton files
/// (`/etc/skel`) and NSS is exactly what battle-tested tooling is for, and
/// reimplementing it here would only be a source of subtle distro-specific
/// bugs. **No `-p` is ever passed** — this platform is SSH-key-only by
/// explicit product decision, so a freshly created account starts with no
/// usable password (`useradd`'s own default: a `!` password-hash field,
/// i.e. login disabled until a key is deployed via [`add_authorized_key`])
/// — that is the intended state, not an oversight.
pub fn create_user(
    name: &str,
    home: Option<&str>,
    shell: Option<&str>,
    create_home: Option<bool>,
    groups: &[String],
) -> Result<ManagedUser> {
    validate_username(name)?;
    if read_accounts()?.iter().any(|u| u.name == name) {
        return Err(UserError::AlreadyExists(name.to_string()));
    }
    if let Some(home) = home {
        if !home.starts_with('/') {
            return Err(UserError::InvalidInput(format!(
                "home must be an absolute path: {home}"
            )));
        }
    }
    let known_groups = read_groups()?;
    for group in groups {
        if !known_groups.iter().any(|(n, _)| n == group) {
            return Err(UserError::UnknownGroup(group.clone()));
        }
    }

    let mut cmd = std::process::Command::new("useradd");
    if create_home == Some(false) {
        cmd.arg("-M");
    } else {
        cmd.arg("-m");
    }
    if let Some(home) = home {
        cmd.arg("-d").arg(home);
    }
    if let Some(shell) = shell {
        cmd.arg("-s").arg(shell);
    }
    if !groups.is_empty() {
        cmd.arg("-G").arg(groups.join(","));
    }
    cmd.arg(name);
    run_command("useradd", &mut cmd)?;

    find_account(name)
}

/// Deletes a Linux account via `userdel`. Refuses uid 0 and any uid below
/// 1000 **unconditionally** — a hard safety rail, not a configurable
/// policy: this deletes a Linux account, not a platform record, and
/// system/service accounts are not this feature's business.
pub fn delete_user(name: &str, remove_home: bool) -> Result<()> {
    validate_username(name)?;
    let account = find_account(name)?;
    if account.uid == 0 || account.is_system {
        return Err(UserError::Protected(
            "refusing to delete a system account".into(),
        ));
    }

    let mut cmd = std::process::Command::new("userdel");
    if remove_home {
        cmd.arg("-r");
    }
    cmd.arg(name);
    run_command("userdel", &mut cmd)
}

/// Locks or unlocks an account (`usermod -L`/`-U`). On most distros with
/// `UsePAM yes` in `sshd_config`, a locked account is refused by
/// `pam_unix`'s account-management check even for an otherwise-successful
/// pubkey authentication — so this is a real "suspend access" control for
/// a key-only account, not a cosmetic flag. The exact behavior still
/// depends on the distro's PAM configuration.
pub fn set_user_locked(name: &str, locked: bool) -> Result<ManagedUser> {
    validate_username(name)?;
    // Ensure the account exists before shelling out, so a typo surfaces as
    // our own NotFound rather than usermod's stderr.
    find_account(name)?;

    let flag = if locked { "-L" } else { "-U" };
    let mut cmd = std::process::Command::new("usermod");
    cmd.arg(flag).arg(name);
    run_command("usermod", &mut cmd)?;

    find_account(name)
}

/// Changes an account's login shell (`usermod -s`). The shell must be an
/// absolute path already listed in `/etc/shells` — the same allowlist
/// `chsh` itself enforces — so a request cannot point a login shell at an
/// arbitrary binary.
pub fn set_user_shell(name: &str, shell: &str) -> Result<ManagedUser> {
    validate_username(name)?;
    if !shell.starts_with('/') {
        return Err(UserError::InvalidInput(format!(
            "shell must be an absolute path: {shell}"
        )));
    }
    let allowed = std::fs::read_to_string("/etc/shells").map_err(UserError::Io)?;
    let known = allowed.lines().any(|line| {
        let line = line.trim();
        !line.is_empty() && !line.starts_with('#') && line == shell
    });
    if !known {
        return Err(UserError::InvalidInput(format!(
            "{shell} is not listed in /etc/shells"
        )));
    }

    let mut cmd = std::process::Command::new("usermod");
    cmd.arg("-s").arg(shell).arg(name);
    run_command("usermod", &mut cmd)?;

    find_account(name)
}

/// Replaces the full supplementary group set (`usermod -G`), never a
/// partial add/remove — `usermod -G` itself always overwrites, and a
/// partial API here would let two operators silently clobber each other's
/// change. An empty `groups` clears every supplementary group; `usermod`
/// still needs `-G` passed with an empty string argument for that, not the
/// flag omitted.
pub fn set_user_groups(name: &str, groups: &[String]) -> Result<ManagedUser> {
    validate_username(name)?;
    let known_groups = read_groups()?;
    for group in groups {
        if !known_groups.iter().any(|(n, _)| n == group) {
            return Err(UserError::UnknownGroup(group.clone()));
        }
    }

    let mut cmd = std::process::Command::new("usermod");
    cmd.arg("-G").arg(groups.join(",")).arg(name);
    run_command("usermod", &mut cmd)?;

    find_account(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn require_root_gates_non_root_callers() {
        let root = UserContext {
            uid: 0,
            name: "root".into(),
            is_root: true,
        };
        let user = UserContext {
            uid: 1000,
            name: "user".into(),
            is_root: false,
        };
        assert!(require_root(&root).is_ok());
        assert!(require_root(&user).is_err());
    }

    #[test]
    fn validate_username_accepts_the_posix_portable_rule() {
        assert!(validate_username("root").is_ok());
        assert!(validate_username("_svc").is_ok());
        assert!(validate_username("a-b_9").is_ok());
        assert!(validate_username("").is_err());
        assert!(validate_username("Bad").is_err());
        assert!(validate_username("9start").is_err());
        assert!(validate_username(&"a".repeat(33)).is_err());
        assert!(validate_username("has space").is_err());
        assert!(validate_username("rm -rf /").is_err());
    }

    #[test]
    fn list_users_includes_root_and_is_sorted_by_uid() {
        let users = list_users().unwrap();
        let root = users.iter().find(|u| u.uid == 0).expect("root exists");
        assert_eq!(root.name, "root");
        assert!(root.is_system);
        assert!(users.windows(2).all(|w| w[0].uid <= w[1].uid));
    }
}
