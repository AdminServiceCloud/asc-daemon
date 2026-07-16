//! Backup storages (DMN-009): where `asc backup create` uploads archives to,
//! and `asc backup restore` pulls them from. One built-in **local** storage
//! always exists (no setup needed); more can be added — S3-compatible, FTP,
//! SFTP — with `asc backup storage add`. Configured storages persist like
//! registry sources (`super::super::pkg::sources`): a system list
//! (`/etc/asc/backup-storages.toml`, root-managed, visible to everyone) and
//! a user list (`~/.config/asc/backup-storages.toml`) that supplements it.
//! The file may hold provider credentials, so it is written 0600.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::daemon::pkg::sources::Scope;

/// Name of the built-in, always-present storage — cannot be added or
/// removed, only pointed elsewhere is not supported in this increment (its
/// directory is fixed: `<data_dir>/backups`).
pub const LOCAL_NAME: &str = "local";

const DEFAULT_SYSTEM_PATH: &str = "/etc/asc/backup-storages.toml";

/// One configured storage beyond the built-in `local` one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageEntry {
    pub name: String,
    #[serde(flatten)]
    pub kind: StorageKind,
}

/// Untagged on purpose, like git auth's `Method` — the TOML stays flat
/// (`type = "s3"` right next to the provider's own fields).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum StorageKind {
    /// A local directory other than the built-in default — e.g. a mounted
    /// external disk or network share.
    Local { dir: PathBuf },
    /// An S3-compatible bucket (AWS S3, MinIO, Backblaze B2, …).
    S3 {
        bucket: String,
        region: String,
        /// Non-AWS endpoint, for S3-compatible providers.
        #[serde(default)]
        endpoint: Option<String>,
        access_key: String,
        secret_key: String,
        #[serde(default)]
        prefix: Option<String>,
    },
    Ftp {
        host: String,
        #[serde(default = "default_ftp_port")]
        port: u16,
        user: String,
        password: String,
        #[serde(default)]
        dir: Option<String>,
    },
    Sftp {
        host: String,
        #[serde(default = "default_sftp_port")]
        port: u16,
        user: String,
        /// Password auth, when no key is given.
        #[serde(default)]
        password: Option<String>,
        /// Private key auth, preferred over a password when both are set.
        #[serde(default)]
        key: Option<PathBuf>,
        #[serde(default)]
        dir: Option<String>,
    },
}

fn default_ftp_port() -> u16 {
    21
}

fn default_sftp_port() -> u16 {
    22
}

impl StorageKind {
    /// Technical kind label for tables (not translated).
    pub fn label(&self) -> &'static str {
        match self {
            StorageKind::Local { .. } => "local",
            StorageKind::S3 { .. } => "s3",
            StorageKind::Ftp { .. } => "ftp",
            StorageKind::Sftp { .. } => "sftp",
        }
    }
}

/// Where `asc backup` reads/writes archives. `push`/`pull` work with a
/// caller-supplied local path (the archive is always built/restored on
/// local disk first — see [`super::create`]/[`super::restore`]); `remote_name`
/// is an opaque identifier the storage assigns meaning to (a file name for
/// local/FTP/SFTP, an object key for S3).
pub trait BackupStorage {
    fn push(&self, local_archive: &Path, remote_name: &str) -> Result<()>;
    fn pull(&self, remote_name: &str, local_dest: &Path) -> Result<()>;
    /// Remote names for one app, oldest first (names are
    /// `<app-id>-<unix-timestamp>.tar.gz`, which sorts chronologically).
    fn list(&self, app_id: &str) -> Result<Vec<String>>;
    fn remove(&self, remote_name: &str) -> Result<()>;
}

/// A plain directory on the local filesystem — the only storage kind that
/// actually works in this increment; S3/FTP/SFTP are configurable but not
/// wired up to a real transfer yet (DMN-009 follow-up).
pub struct Local {
    pub dir: PathBuf,
}

impl BackupStorage for Local {
    fn push(&self, local_archive: &Path, remote_name: &str) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("cannot create backup directory {}", self.dir.display()))?;
        let dest = self.dir.join(remote_name);
        fs::copy(local_archive, &dest)
            .with_context(|| format!("cannot write backup {}", dest.display()))?;
        Ok(())
    }

    fn pull(&self, remote_name: &str, local_dest: &Path) -> Result<()> {
        let src = self.dir.join(remote_name);
        fs::copy(&src, local_dest)
            .with_context(|| format!("cannot read backup {}", src.display()))?;
        Ok(())
    }

    fn list(&self, app_id: &str) -> Result<Vec<String>> {
        let prefix = format!("{app_id}-");
        let mut names = match fs::read_dir(&self.dir) {
            Ok(entries) => entries
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|name| name.starts_with(&prefix) && name.ends_with(".tar.gz"))
                .collect::<Vec<_>>(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("cannot list backups in {}", self.dir.display()));
            }
        };
        names.sort();
        Ok(names)
    }

    fn remove(&self, remote_name: &str) -> Result<()> {
        let path = self.dir.join(remote_name);
        fs::remove_file(&path).with_context(|| format!("cannot remove backup {}", path.display()))
    }
}

/// A provider that is configured but not implemented yet — every operation
/// fails with the same clear message instead of silently doing nothing.
struct NotImplemented(&'static str);

impl BackupStorage for NotImplemented {
    fn push(&self, _: &Path, _: &str) -> Result<()> {
        bail!(
            "{} backup storage is not implemented yet (DMN-009) — use the 'local' storage for now",
            self.0
        )
    }
    fn pull(&self, _: &str, _: &Path) -> Result<()> {
        bail!(
            "{} backup storage is not implemented yet (DMN-009) — use the 'local' storage for now",
            self.0
        )
    }
    fn list(&self, _: &str) -> Result<Vec<String>> {
        bail!(
            "{} backup storage is not implemented yet (DMN-009) — use the 'local' storage for now",
            self.0
        )
    }
    fn remove(&self, _: &str) -> Result<()> {
        bail!(
            "{} backup storage is not implemented yet (DMN-009) — use the 'local' storage for now",
            self.0
        )
    }
}

/// Build the storage implementation for one entry.
pub fn open(kind: &StorageKind) -> Box<dyn BackupStorage> {
    match kind {
        StorageKind::Local { dir } => Box::new(Local { dir: dir.clone() }),
        StorageKind::S3 { .. } => Box::new(NotImplemented("S3")),
        StorageKind::Ftp { .. } => Box::new(NotImplemented("FTP")),
        StorageKind::Sftp { .. } => Box::new(NotImplemented("SFTP")),
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StoragesFile {
    #[serde(default, rename = "storage")]
    storages: Vec<StorageEntry>,
}

/// Configured storages beyond the built-in `local` one; edits apply to
/// `scope` (mirrors [`crate::daemon::pkg::sources::SourceList`]).
#[derive(Debug, Clone)]
pub struct StorageList {
    system: Vec<StorageEntry>,
    user: Vec<StorageEntry>,
    scope: Scope,
}

impl StorageList {
    /// System file: `$ASC_BACKUP_STORAGES` override or the platform default.
    pub fn system_path() -> PathBuf {
        std::env::var_os("ASC_BACKUP_STORAGES")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_SYSTEM_PATH))
    }

    /// User file: `$ASC_USER_BACKUP_STORAGES` override or
    /// `~/.config/asc/backup-storages.toml`.
    pub fn user_path() -> Result<PathBuf> {
        if let Some(path) = std::env::var_os("ASC_USER_BACKUP_STORAGES") {
            return Ok(PathBuf::from(path));
        }
        let home = std::env::var_os("HOME").context("cannot determine home directory ($HOME)")?;
        Ok(PathBuf::from(home).join(".config/asc/backup-storages.toml"))
    }

    pub fn load() -> Result<Self> {
        Self::load_with(Scope::current())
    }

    pub fn load_with(scope: Scope) -> Result<Self> {
        let system = read_storages(&Self::system_path())?.unwrap_or_default();
        let user = match scope {
            Scope::System => Vec::new(),
            Scope::User => read_storages(&Self::user_path()?)?.unwrap_or_default(),
        };
        Ok(Self {
            system,
            user,
            scope,
        })
    }

    /// Persist the editable list with owner-only permissions (may hold
    /// provider credentials).
    pub fn save(&self) -> Result<()> {
        let (path, storages) = match self.scope {
            Scope::System => (Self::system_path(), &self.system),
            Scope::User => (Self::user_path()?, &self.user),
        };
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            fs::create_dir_all(dir)
                .with_context(|| format!("cannot create directory {}", dir.display()))?;
        }
        let raw = toml::to_string_pretty(&StoragesFile {
            storages: storages.clone(),
        })
        .context("cannot serialize backup storages")?;
        fs::write(&path, raw)
            .with_context(|| format!("cannot write backup storages file {}", path.display()))?;
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                .with_context(|| format!("cannot set permissions on {}", path.display()))?;
        }
        Ok(())
    }

    /// All storage names, `local` first, then configured ones in priority
    /// order (system before user, like [`super::super::pkg::sources`]).
    pub fn names(&self) -> Vec<String> {
        let mut names = vec![LOCAL_NAME.to_string()];
        for entry in &self.system {
            names.push(entry.name.clone());
        }
        for entry in &self.user {
            if !self.system.iter().any(|e| e.name == entry.name) {
                names.push(entry.name.clone());
            }
        }
        names
    }

    /// The entry for `name`, or `None` for `local` (the caller resolves the
    /// built-in storage itself — it is not stored as an entry) or an
    /// unknown name.
    pub fn get(&self, name: &str) -> Option<&StorageEntry> {
        self.user
            .iter()
            .find(|e| e.name == name)
            .or_else(|| self.system.iter().find(|e| e.name == name))
    }

    /// Add a configured storage; `local` is reserved for the built-in one.
    pub fn add(&mut self, name: &str, kind: StorageKind) -> Result<()> {
        if name == LOCAL_NAME {
            bail!("'{LOCAL_NAME}' is the built-in storage name and cannot be reused");
        }
        if self.names().iter().any(|n| n == name) {
            bail!("storage '{name}' already exists");
        }
        let target = match self.scope {
            Scope::System => &mut self.system,
            Scope::User => &mut self.user,
        };
        target.push(StorageEntry {
            name: name.to_string(),
            kind,
        });
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> Result<()> {
        if name == LOCAL_NAME {
            bail!("'{LOCAL_NAME}' is the built-in storage and cannot be removed");
        }
        let target = match self.scope {
            Scope::System => &mut self.system,
            Scope::User => &mut self.user,
        };
        let before = target.len();
        target.retain(|e| e.name != name);
        if target.len() == before {
            if self.scope == Scope::User && self.system.iter().any(|e| e.name == name) {
                bail!("storage '{name}' is a system storage (managed by root; run with sudo)");
            }
            bail!("storage '{name}' not found");
        }
        Ok(())
    }
}

fn read_storages(path: &Path) -> Result<Option<Vec<StorageEntry>>> {
    match fs::read_to_string(path) {
        Ok(raw) => {
            let file: StoragesFile = toml::from_str(&raw)
                .with_context(|| format!("invalid backup storages file {}", path.display()))?;
            Ok(Some(file.storages))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => {
            Err(e).with_context(|| format!("cannot read backup storages file {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(system: &[&str], user: &[&str], scope: Scope) -> StorageList {
        let make = |names: &[&str]| {
            names
                .iter()
                .map(|n| StorageEntry {
                    name: n.to_string(),
                    kind: StorageKind::Local {
                        dir: PathBuf::from("/tmp/x"),
                    },
                })
                .collect()
        };
        StorageList {
            system: make(system),
            user: make(user),
            scope,
        }
    }

    #[test]
    fn local_is_always_first_and_implicit() {
        let l = list(&[], &[], Scope::User);
        assert_eq!(l.names(), vec![LOCAL_NAME.to_string()]);
        assert!(l.get(LOCAL_NAME).is_none(), "local is not a stored entry");
    }

    #[test]
    fn local_name_is_reserved() {
        let mut l = list(&[], &[], Scope::System);
        let err = l
            .add(LOCAL_NAME, StorageKind::Local { dir: "/x".into() })
            .unwrap_err();
        assert!(err.to_string().contains("built-in"));
        assert!(l.remove(LOCAL_NAME).is_err());
    }

    #[test]
    fn system_storages_cannot_be_shadowed_or_removed_by_a_user() {
        let mut l = list(&["s3-main"], &[], Scope::User);
        let err = l
            .add("s3-main", StorageKind::Local { dir: "/x".into() })
            .unwrap_err();
        assert!(err.to_string().contains("already exists"));
        let err = l.remove("s3-main").unwrap_err().to_string();
        assert!(err.to_lowercase().contains("sudo"), "got: {err}");
    }

    #[test]
    fn user_adds_and_removes_own_storages() {
        let mut l = list(&[], &[], Scope::User);
        l.add("mine", StorageKind::Local { dir: "/x".into() })
            .unwrap();
        assert_eq!(l.names(), vec![LOCAL_NAME.to_string(), "mine".to_string()]);
        l.remove("mine").unwrap();
        assert_eq!(l.names(), vec![LOCAL_NAME.to_string()]);
    }
}
