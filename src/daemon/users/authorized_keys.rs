//! `~/.ssh/authorized_keys` management for a managed account (DMN-099):
//! idempotent add, fingerprint-based remove, and a deliberately limited
//! listing parser — see [`list_authorized_keys`]'s doc comment for what it
//! does not attempt to parse.
//!
//! A user's home directory is always resolved by looking the account up
//! through [`super::read_accounts`] — never trusted from a client-supplied
//! path — so every function here starts from the account, not a string.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use super::{Result, UserError, read_accounts};

/// Prefixes that mark the first token of an `authorized_keys` line as a
/// key type rather than an options string (`command="...",...`) — the
/// overwhelmingly common case for machine-managed keys.
const KEY_TYPE_PREFIXES: &[&str] = &["ssh-", "ecdsa-", "sk-"];

/// One parsed `authorized_keys` entry.
pub struct AuthorizedKey {
    pub fingerprint: String,
    pub key_type: String,
    pub comment: String,
}

fn is_key_type(token: &str) -> bool {
    KEY_TYPE_PREFIXES.iter().any(|p| token.starts_with(p))
}

/// Resolves `user` to `(uid, gid, home)` via the account list — `NotFound`
/// if no such account exists.
fn resolve_account(user: &str) -> Result<(u32, u32, PathBuf)> {
    let account = read_accounts()?
        .into_iter()
        .find(|u| u.name == user)
        .ok_or_else(|| UserError::NotFound(user.to_string()))?;
    Ok((account.uid, account.gid, PathBuf::from(account.home)))
}

fn authorized_keys_path(home: &Path) -> PathBuf {
    home.join(".ssh").join("authorized_keys")
}

fn chmod(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(UserError::Io)
}

fn chown(path: &Path, uid: u32, gid: u32) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let cpath = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| UserError::InvalidInput("path contains a NUL byte".into()))?;
    // SAFETY: cpath is a valid, NUL-terminated C string for the duration of
    // the call; chown cannot corrupt Rust-side state.
    let rc = unsafe { libc::chown(cpath.as_ptr(), uid, gid) };
    if rc != 0 {
        return Err(UserError::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Runs `ssh-keygen -lf -` over `public_key_line` and returns just the
/// `SHA256:...` fingerprint token. This doubles as input validation for
/// [`add_authorized_key`]: an invalid key line simply fails to fingerprint.
fn fingerprint_of(public_key_line: &str) -> Result<String> {
    let invalid = || UserError::InvalidInput("not a valid SSH public key".into());

    let mut child = std::process::Command::new("ssh-keygen")
        .arg("-lf")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(UserError::Io)?;
    {
        let stdin = child.stdin.as_mut().expect("stdin was piped");
        stdin
            .write_all(public_key_line.as_bytes())
            .and_then(|()| stdin.write_all(b"\n"))
            .map_err(UserError::Io)?;
    }
    let output = child.wait_with_output().map_err(UserError::Io)?;
    if !output.status.success() {
        return Err(invalid());
    }
    // "256 SHA256:xxxxxxxx comment (ED25519)"
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .nth(1)
        .filter(|token| token.starts_with("SHA256:"))
        .map(str::to_string)
        .ok_or_else(invalid)
}

/// Lists `user`'s authorized keys. A missing `authorized_keys` file is not
/// an error — a fresh account simply has none — and returns an empty list.
///
/// Only lines whose first whitespace-separated token is a recognized key
/// type (`ssh-`, `ecdsa-`, `sk-` — the overwhelmingly common case for a
/// machine-managed file) are parsed. A line that instead starts with an
/// options string (e.g. `command="...",... ssh-ed25519 ...`) is skipped
/// rather than attempting to parse the options grammar; a line that fails
/// to fingerprint (hand-edited garbage) is likewise skipped rather than
/// failing the whole call.
pub fn list_authorized_keys(user: &str) -> Result<Vec<AuthorizedKey>> {
    let (_, _, home) = resolve_account(user)?;
    let path = authorized_keys_path(&home);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(UserError::Io(err)),
    };

    let mut keys = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(key_type) = parts.next() else {
            continue;
        };
        if !is_key_type(key_type) {
            continue;
        }
        let Some(blob) = parts.next() else { continue };
        let comment = parts.collect::<Vec<_>>().join(" ");
        let Ok(fingerprint) = fingerprint_of(&format!("{key_type} {blob}")) else {
            continue;
        };
        keys.push(AuthorizedKey {
            fingerprint,
            key_type: key_type.to_string(),
            comment,
        });
    }
    Ok(keys)
}

/// Idempotently appends `public_key` to `user`'s `authorized_keys`: if a
/// key with the same fingerprint is already present, that existing entry
/// is returned unchanged rather than duplicating the line.
pub fn add_authorized_key(user: &str, public_key: &str) -> Result<AuthorizedKey> {
    let (uid, gid, home) = resolve_account(user)?;

    let trimmed = public_key.trim();
    if trimmed.contains('\n') || trimmed.contains('\r') {
        return Err(UserError::InvalidInput(
            "public_key must be a single line".into(),
        ));
    }
    let mut tokens = trimmed.split_whitespace();
    let key_type = tokens.next().unwrap_or_default();
    if !is_key_type(key_type) {
        return Err(UserError::InvalidInput("not a valid SSH public key".into()));
    }
    // Also validates the key is well-formed.
    let fingerprint = fingerprint_of(trimmed)?;

    if let Some(existing) = list_authorized_keys(user)?
        .into_iter()
        .find(|k| k.fingerprint == fingerprint)
    {
        return Ok(existing);
    }

    let ssh_dir = home.join(".ssh");
    std::fs::create_dir_all(&ssh_dir).map_err(UserError::Io)?;
    chmod(&ssh_dir, 0o700)?;
    chown(&ssh_dir, uid, gid)?;

    let path = authorized_keys_path(&home);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(UserError::Io)?;
    chmod(&path, 0o600)?;
    chown(&path, uid, gid)?;

    file.write_all(trimmed.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .map_err(UserError::Io)?;

    let key_type = key_type.to_string();
    tokens.next(); // the base64 blob — not part of the comment
    let comment = tokens.collect::<Vec<_>>().join(" ");
    Ok(AuthorizedKey {
        fingerprint,
        key_type,
        comment,
    })
}

/// Removes the key matching `fingerprint` from `user`'s `authorized_keys`.
/// A missing file is treated as already-removed (`Ok(())`), matching the
/// idempotent spirit of [`add_authorized_key`]. Lines that cannot be
/// parsed as a key (options-string lines, hand-edited garbage) are always
/// kept untouched, whether or not they happen to match by accident.
pub fn remove_authorized_key(user: &str, fingerprint: &str) -> Result<()> {
    let (_, _, home) = resolve_account(user)?;
    let path = authorized_keys_path(&home);

    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(UserError::Io(err)),
    };
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(&path)
            .map_err(UserError::Io)?
            .permissions()
            .mode()
            & 0o7777
    };

    let mut kept = String::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        let keep = if trimmed.is_empty() || trimmed.starts_with('#') {
            true
        } else {
            let mut parts = trimmed.split_whitespace();
            match parts.next() {
                Some(key_type) if is_key_type(key_type) => {
                    let blob = parts.next().unwrap_or_default();
                    match fingerprint_of(&format!("{key_type} {blob}")) {
                        Ok(fp) => fp != fingerprint,
                        // Unparseable: leave it untouched rather than risk
                        // dropping a line we cannot actually identify.
                        Err(_) => true,
                    }
                }
                // Options-string / unrecognized line: never touched.
                _ => true,
            }
        };
        if keep {
            kept.push_str(line);
            kept.push('\n');
        }
    }

    let tmp_path = path.with_extension("tmp");
    std::fs::write(&tmp_path, kept).map_err(UserError::Io)?;
    chmod(&tmp_path, mode)?;
    std::fs::rename(&tmp_path, &path).map_err(UserError::Io)?;
    Ok(())
}
