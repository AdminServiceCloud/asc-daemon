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
    /// Who owns this entry when it was not added by hand (DMN-115): the
    /// platform pushes its organization storages as `managed_by =
    /// "platform"` and may replace or remove them; `None` for operator
    /// entries, which a platform push never touches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_by: Option<String>,
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

/// One archive on a storage: its remote name and size in bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupObject {
    pub name: String,
    pub size: u64,
}

impl BackupObject {
    /// Creation time encoded in the name (`<app-id>-<unix-ts>.tar.gz`),
    /// `None` for a name that does not follow the convention.
    pub fn created_unix(&self) -> Option<i64> {
        created_unix(&self.name)
    }
}

/// See [`BackupObject::created_unix`].
pub fn created_unix(name: &str) -> Option<i64> {
    let stem = name.strip_suffix(".tar.gz")?;
    let (_, ts) = stem.rsplit_once('-')?;
    ts.parse().ok()
}

/// Where `asc backup` reads/writes archives. `push`/`pull` work with a
/// caller-supplied local path (the archive is always built/restored on
/// local disk first — see [`super::create`]/[`super::restore`]); `remote_name`
/// is an opaque identifier the storage assigns meaning to (a file name for
/// local/FTP/SFTP, an object key for S3).
pub trait BackupStorage {
    fn push(&self, local_archive: &Path, remote_name: &str) -> Result<()>;
    fn pull(&self, remote_name: &str, local_dest: &Path) -> Result<()>;
    /// Archives of one app, oldest first (names are
    /// `<app-id>-<unix-timestamp>.tar.gz`, which sorts chronologically).
    /// Only names of exactly this app: `demo` must not pick up `demo-2`'s
    /// archives (see [`belongs_to`]).
    fn list(&self, app_id: &str) -> Result<Vec<BackupObject>>;
    fn remove(&self, remote_name: &str) -> Result<()>;
}

/// Whether `name` is an archive of `app_id` and nothing else: the
/// `<app-id>-` prefix alone would also match app `demo-2` when listing
/// `demo`, so the remainder must be exactly `<digits>.tar.gz`.
pub fn belongs_to(name: &str, app_id: &str) -> bool {
    name.strip_prefix(app_id)
        .and_then(|rest| rest.strip_prefix('-'))
        .and_then(|rest| rest.strip_suffix(".tar.gz"))
        .is_some_and(|ts| !ts.is_empty() && ts.bytes().all(|b| b.is_ascii_digit()))
}

/// A plain directory on the local filesystem. S3 lives in [`super::s3`];
/// FTP/SFTP are configurable but not wired up to a real transfer yet.
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

    fn list(&self, app_id: &str) -> Result<Vec<BackupObject>> {
        let mut names = match fs::read_dir(&self.dir) {
            Ok(entries) => entries
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let name = e.file_name().into_string().ok()?;
                    if !belongs_to(&name, app_id) {
                        return None;
                    }
                    let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                    Some(BackupObject { name, size })
                })
                .collect::<Vec<_>>(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("cannot list backups in {}", self.dir.display()));
            }
        };
        names.sort_by(|a, b| a.name.cmp(&b.name));
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
            "{} backup storage is not implemented yet — use the 'local' or 's3' storage for now",
            self.0
        )
    }
    fn pull(&self, _: &str, _: &Path) -> Result<()> {
        bail!(
            "{} backup storage is not implemented yet — use the 'local' or 's3' storage for now",
            self.0
        )
    }
    fn list(&self, _: &str) -> Result<Vec<BackupObject>> {
        bail!(
            "{} backup storage is not implemented yet — use the 'local' or 's3' storage for now",
            self.0
        )
    }
    fn remove(&self, _: &str) -> Result<()> {
        bail!(
            "{} backup storage is not implemented yet — use the 'local' or 's3' storage for now",
            self.0
        )
    }
}

/// Build the storage implementation for one entry.
pub fn open(kind: &StorageKind) -> Result<Box<dyn BackupStorage>> {
    Ok(match kind {
        StorageKind::Local { dir } => Box::new(Local { dir: dir.clone() }),
        StorageKind::S3 {
            bucket,
            region,
            endpoint,
            access_key,
            secret_key,
            prefix,
        } => Box::new(super::s3::S3::new(
            endpoint.as_deref(),
            region,
            bucket,
            access_key,
            secret_key,
            prefix.as_deref(),
        )?),
        StorageKind::Ftp { .. } => Box::new(NotImplemented("FTP")),
        StorageKind::Sftp { .. } => Box::new(NotImplemented("SFTP")),
    })
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
            managed_by: None,
            kind,
        });
        Ok(())
    }

    /// Add or replace a storage in the editable scope (DMN-115, the
    /// platform's push). Replacing an operator entry with a managed one (or
    /// the reverse) is refused: a push must never silently take over — or
    /// hand back — something it does not own.
    pub fn upsert(&mut self, entry: StorageEntry) -> Result<()> {
        if entry.name == LOCAL_NAME {
            bail!("'{LOCAL_NAME}' is the built-in storage name and cannot be reused");
        }
        let target = match self.scope {
            Scope::System => &mut self.system,
            Scope::User => &mut self.user,
        };
        if let Some(existing) = target.iter_mut().find(|e| e.name == entry.name) {
            if existing.managed_by != entry.managed_by {
                bail!(
                    "storage '{}' exists and is managed by {} — refusing to replace it",
                    entry.name,
                    existing.managed_by.as_deref().unwrap_or("the operator")
                );
            }
            *existing = entry;
        } else {
            target.push(entry);
        }
        Ok(())
    }

    /// Every configured entry visible here, system first, then user entries
    /// not shadowed by a system one (the built-in `local` is not included).
    pub fn entries(&self) -> Vec<&StorageEntry> {
        let mut out: Vec<&StorageEntry> = self.system.iter().collect();
        for entry in &self.user {
            if !self.system.iter().any(|e| e.name == entry.name) {
                out.push(entry);
            }
        }
        out
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
                    managed_by: None,
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
    fn archive_names_belong_to_exactly_one_app() {
        assert!(belongs_to("demo-1767225600.tar.gz", "demo"));
        assert!(!belongs_to("demo-2-1767225600.tar.gz", "demo"));
        assert!(belongs_to("demo-2-1767225600.tar.gz", "demo-2"));
        assert!(!belongs_to("demo-.tar.gz", "demo"));
        assert!(!belongs_to("demo-1767225600.tar", "demo"));
        assert_eq!(
            created_unix("demo-2-1767225600.tar.gz"),
            Some(1_767_225_600)
        );
        assert_eq!(created_unix("junk"), None);
    }

    #[test]
    fn upsert_replaces_only_what_it_owns() {
        let mut l = list(&["manual"], &[], Scope::System);
        let managed = |name: &str, dir: &str| StorageEntry {
            name: name.into(),
            managed_by: Some("platform".into()),
            kind: StorageKind::Local { dir: dir.into() },
        };
        l.upsert(managed("platform-1", "/a")).unwrap();
        l.upsert(managed("platform-1", "/b")).unwrap();
        assert_eq!(l.entries().len(), 2);
        match &l.get("platform-1").unwrap().kind {
            StorageKind::Local { dir } => assert_eq!(dir, &PathBuf::from("/b")),
            other => panic!("unexpected {other:?}"),
        }
        let err = l.upsert(managed("manual", "/c")).unwrap_err().to_string();
        assert!(err.contains("refusing"), "{err}");
        assert!(l.upsert(managed(LOCAL_NAME, "/c")).is_err());
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
