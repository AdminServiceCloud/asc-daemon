//! The daemon's local database (DMN-160): an SQLite index of installed
//! apps, `<data_dir>/index.db`.
//!
//! The index mirrors every app's state directory: `meta.json`, split into
//! columns for lookups, and the content of every other top-level file of
//! `.asc/` (`settings.json`, `compose.override.yaml`, …) with a stamp of
//! the file it was read from. The files stay the source of truth — backups,
//! clones and manual recovery work on them — and the index is always
//! rebuildable from them.
//!
//! Keeping the two in sync:
//!
//! - **write-through** — [`AppStore::save`](super::AppStore::save) and
//!   `remove`, `SettingValues::save` and the compose override writer call
//!   [`changed`] / [`AppIndex::sync_app`] right after the file lands;
//! - **watcher** (Linux, daemon only) — inotify on the apps root, each app
//!   directory, its `.asc/` and `repository/.git` re-syncs one app;
//! - **verify** — a `stat`-only comparison of every app's stamps against
//!   the index: once a minute in the daemon, and before every read in a
//!   process nothing else keeps the index current for (the CLI working
//!   without a daemon);
//! - **rebuild** — a full scan when there is no database, the schema or the
//!   apps directory changed, or the file is broken.
//!
//! A sync reads the files *inside* its write transaction (`BEGIN
//! IMMEDIATE`), so two concurrent syncs — two threads, or the CLI and the
//! daemon — can never store an older copy over a newer one.

mod scan;
mod schema;
#[cfg(target_os = "linux")]
mod watch;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, ErrorCode, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use super::meta::{AppMeta, DesiredState, Runtime, validate_id};
use crate::daemon::config::Config;

pub use scan::MAX_MIRRORED_BYTES;
use scan::{AppStamps, ReadError};

/// File name of the database inside the data directory.
pub const FILE: &str = "index.db";

/// Escape hatch: a non-empty value other than `0` keeps every process on
/// the plain filesystem listing, as before the index existed.
pub const DISABLE_ENV: &str = "ASC_NO_INDEX";

/// How often the daemon verifies the index against the files on top of
/// what its watcher reports.
pub const VERIFY_INTERVAL: Duration = Duration::from_secs(60);

/// The local database of one apps directory.
pub struct AppIndex {
    path: PathBuf,
    root: PathBuf,
    conn: Mutex<Connection>,
    /// Something in this process keeps the index current (the daemon's
    /// watcher and periodic verify); reads then trust it as is. Otherwise
    /// every read verifies first — a stat per file, so the CLI working on
    /// its own still sees manual edits and a daemon that was stopped.
    maintained: AtomicBool,
    /// The inotify watcher of this process is running.
    watching: AtomicBool,
    /// Stamps of apps whose `meta.json` could not be used, so verify
    /// retries (and warns about) them only once they change.
    broken: Mutex<HashMap<String, AppStamps>>,
}

/// One indexed app, as the listing needs it.
#[derive(Debug, Clone)]
pub struct IndexedApp {
    pub meta: AppMeta,
    /// HEAD of `repository/` when the app was last synced; `None` when it is
    /// not a git checkout.
    pub commit: Option<String>,
    /// Last observed runtime state (`running` / `stopped`), if any.
    pub observed: Option<String>,
}

/// One indexed file of an app's `.asc/`.
#[derive(Debug, PartialEq, Eq)]
pub enum StateFile {
    /// The app is not in the index: read the file instead.
    NotIndexed,
    /// The app is indexed and has no such file.
    Absent,
    /// The file is indexed without its content (larger than
    /// [`MAX_MIRRORED_BYTES`]): read it from disk.
    TooLarge,
    Content(Vec<u8>),
}

/// `asc db status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexStatus {
    pub path: PathBuf,
    pub apps_root: PathBuf,
    pub schema_version: i64,
    pub apps: u64,
    pub files: u64,
    /// The database file plus its write-ahead log.
    pub size_bytes: u64,
    /// Unix ms; `None` before the first one.
    pub last_full_scan: Option<i64>,
    pub last_verify: Option<i64>,
    pub watching: bool,
}

/// `asc db verify`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyReport {
    pub checked: u64,
    pub added: u64,
    pub updated: u64,
    pub removed: u64,
    pub millis: u64,
}

impl VerifyReport {
    pub fn changed(&self) -> bool {
        self.added + self.updated + self.removed > 0
    }
}

/// `asc db rebuild`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebuildReport {
    pub apps: u64,
    /// Directories that hold a `meta.json` the index cannot use.
    pub skipped: u64,
    pub millis: u64,
}

/// What a sync did to one app's rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncOutcome {
    Added,
    Updated,
    Removed,
    Unchanged,
}

/// Why `asc db` (and `/v1/db`) refuses a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbError {
    /// The database covers every app of the node: only root manages it.
    RootOnly,
    /// No database is in use for this apps directory (disabled, or its file
    /// could not be opened — the daemon log says why).
    Disabled,
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use crate::daemon::i18n::{Msg, t};
        f.write_str(match self {
            DbError::RootOnly => t(Msg::DbRootOnly),
            DbError::Disabled => t(Msg::DbDisabled),
        })
    }
}

impl std::error::Error for DbError {}

/// Why a database file cannot be used as it is.
enum OpenError {
    /// Broken, foreign or newer than this build: set aside and start over.
    Unusable(String),
    Other(anyhow::Error),
}

impl From<rusqlite::Error> for OpenError {
    fn from(err: rusqlite::Error) -> Self {
        if is_unusable(&err) {
            OpenError::Unusable(err.to_string())
        } else {
            OpenError::Other(err.into())
        }
    }
}

fn is_unusable(err: &rusqlite::Error) -> bool {
    matches!(
        err.sqlite_error_code(),
        Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase)
    )
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// Lexical normalization (no symlink resolution: the directory may not
/// exist yet), so `/asc/apps/` and `/asc/apps` name the same index.
fn normalize(path: &Path) -> PathBuf {
    path.components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect()
}

/// Create the database file `0600` before SQLite writes anything into it:
/// it holds copies of `settings.json`, which may carry secrets. SQLite
/// gives the `-wal`/`-shm` files the mode of the database file.
fn ensure_private_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    let mode = fs::metadata(path)?.permissions().mode();
    if mode & 0o077 != 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("cannot restrict {}", path.display()))?;
    }
    Ok(())
}

/// Move a database that cannot be used out of the way, with its WAL, as
/// `index.db.broken-<unix seconds>`: kept for a post-mortem, never read
/// again.
fn quarantine(path: &Path, reason: &str) -> Result<PathBuf> {
    let stamp = now_ms() / 1000;
    let target = path.with_file_name(format!("{FILE}.broken-{stamp}"));
    fs::rename(path, &target)
        .with_context(|| format!("cannot move the broken database {}", path.display()))?;
    for suffix in ["-wal", "-shm"] {
        let side = path.with_file_name(format!("{FILE}{suffix}"));
        let _ = fs::rename(
            &side,
            target.with_file_name(format!("{FILE}.broken-{stamp}{suffix}")),
        );
    }
    warn!(path = %path.display(), moved_to = %target.display(), reason, "app index database is unusable, rebuilding it");
    Ok(target)
}

fn desired_label(state: DesiredState) -> &'static str {
    match state {
        DesiredState::Running => "running",
        DesiredState::Stopped => "stopped",
    }
}

/// Insert or replace one app's rows from a snapshot read from disk.
fn write_app(
    conn: &Connection,
    snapshot: &scan::AppSnapshot,
    commit: Option<&str>,
) -> rusqlite::Result<()> {
    let meta = &snapshot.meta;
    let (container, project) = match &meta.runtime {
        Runtime::Docker { container, .. } => (Some(container.as_str()), None),
        Runtime::Compose { project, .. } => (None, Some(project.as_str())),
        _ => (None, None),
    };
    // Serialized from the parsed meta rather than copied from the file:
    // what the listing parses back is then guaranteed to be valid.
    let meta_json = serde_json::to_string(meta)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    conn.execute(
        "INSERT INTO apps (id, uuid, owner_uid, custom_name, custom_name_ci, runtime_kind,
                           container_name, compose_project, desired_state, meta_json,
                           commit_sha, git_stamp, synced_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT(id) DO UPDATE SET
           uuid = excluded.uuid, owner_uid = excluded.owner_uid,
           custom_name = excluded.custom_name, custom_name_ci = excluded.custom_name_ci,
           runtime_kind = excluded.runtime_kind, container_name = excluded.container_name,
           compose_project = excluded.compose_project, desired_state = excluded.desired_state,
           meta_json = excluded.meta_json, commit_sha = excluded.commit_sha,
           git_stamp = excluded.git_stamp, synced_at = excluded.synced_at",
        params![
            meta.id,
            meta.uuid,
            meta.owner.uid as i64,
            meta.custom_name,
            meta.custom_name.as_deref().map(str::to_lowercase),
            meta.runtime.kind(),
            container,
            project,
            desired_label(meta.desired_state),
            meta_json,
            commit,
            snapshot.git_stamp,
            now_ms(),
        ],
    )?;
    conn.execute("DELETE FROM app_files WHERE app_id = ?1", [&meta.id])?;
    let mut insert = conn.prepare_cached(
        "INSERT INTO app_files (app_id, name, content, size, stamp) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for file in &snapshot.files {
        insert.execute(params![
            meta.id,
            file.name,
            file.content,
            file.size as i64,
            file.stamp
        ])?;
    }
    Ok(())
}

fn head_commit(root: &Path, id: &str) -> Option<String> {
    crate::daemon::pkg::gitref::head_commit(&root.join(id).join("repository"))
}

impl AppIndex {
    /// Open (creating or rebuilding as needed) the database at `path` for the
    /// apps under `root`. `owner` is the daemon: it also runs an integrity
    /// check and an initial verify, the way it is the one keeping the index
    /// current afterwards.
    pub fn open(path: &Path, root: &Path, owner: bool) -> Result<Self> {
        let index = match Self::open_once(path, root, owner) {
            Ok(index) => index,
            Err(OpenError::Unusable(reason)) => {
                quarantine(path, &reason)?;
                match Self::open_once(path, root, false) {
                    Ok(index) => index,
                    Err(OpenError::Unusable(reason)) => bail!("{reason}"),
                    Err(OpenError::Other(err)) => return Err(err),
                }
            }
            Err(OpenError::Other(err)) => return Err(err),
        };
        Ok(index)
    }

    fn open_once(path: &Path, root: &Path, owner: bool) -> Result<Self, OpenError> {
        ensure_private_file(path).map_err(OpenError::Other)?;
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        // The first statement is what reads the file header: a foreign file
        // fails here with NotADatabase.
        let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            debug!(path = %path.display(), mode, "app index is not in WAL mode");
        }
        conn.execute_batch("PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;")?;
        if owner {
            let check: String = conn.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
            if check != "ok" {
                return Err(OpenError::Unusable(format!("quick_check: {check}")));
            }
        }
        let mut rebuild = match schema::prepare(&conn)? {
            schema::Prepared::Ready => false,
            schema::Prepared::Empty => true,
            schema::Prepared::Newer(version) => {
                return Err(OpenError::Unusable(format!(
                    "written by a newer daemon (schema v{version})"
                )));
            }
        };
        let root = normalize(root);
        let stored_root = schema::get(&conn, "apps_root")?;
        if stored_root.as_deref() != Some(&*root.to_string_lossy()) {
            if let Some(previous) = stored_root {
                if owner {
                    info!(previous, current = %root.display(), "apps directory changed, rebuilding the app index");
                } else {
                    debug!(previous, current = %root.display(), "apps directory changed, rebuilding the app index");
                }
            }
            rebuild = true;
        }
        let index = Self {
            path: path.to_path_buf(),
            root,
            conn: Mutex::new(conn),
            maintained: AtomicBool::new(false),
            watching: AtomicBool::new(false),
            broken: Mutex::new(HashMap::new()),
        };
        if rebuild {
            let report = index.rebuild().map_err(OpenError::Other)?;
            // The daemon's log records it; a CLI command keeps its output
            // to what the user asked for.
            if owner {
                info!(path = %index.path.display(), apps = report.apps, millis = report.millis, "app index built");
            } else {
                debug!(path = %index.path.display(), apps = report.apps, millis = report.millis, "app index built");
            }
        } else if owner {
            let report = index.verify().map_err(OpenError::Other)?;
            if report.changed() {
                info!(
                    added = report.added,
                    updated = report.updated,
                    removed = report.removed,
                    "app index caught up with the files"
                );
            }
        }
        Ok(index)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        // A panic mid-transaction rolls the transaction back when it is
        // dropped; the connection itself stays usable.
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Mark the index as kept current by this process (the daemon).
    pub(crate) fn set_maintained(&self) {
        self.maintained.store(true, Ordering::Relaxed);
    }

    pub fn watching(&self) -> bool {
        self.watching.load(Ordering::Relaxed)
    }

    /// Verify first unless this process keeps the index current.
    fn ensure_fresh(&self) {
        if self.maintained.load(Ordering::Relaxed) {
            return;
        }
        if let Err(err) = self.verify() {
            warn!(error = %format!("{err:#}"), "cannot verify the app index against the files");
        }
    }

    // ── Reads ───────────────────────────────────────────────────────────

    /// Every indexed app, sorted by id, with its commit and last observed
    /// state.
    pub fn apps(&self) -> Result<Vec<IndexedApp>> {
        self.ensure_fresh();
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT id, meta_json, commit_sha, observed_state FROM apps ORDER BY id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?;
        let mut apps = Vec::new();
        for row in rows {
            let (id, raw, commit, observed) = row?;
            match serde_json::from_str::<AppMeta>(&raw) {
                Ok(meta) => apps.push(IndexedApp {
                    meta,
                    commit,
                    observed,
                }),
                // Written by this module from a parsed meta, so this means a
                // hand-edited database: skip the row like a broken
                // meta.json, the next sync of the app replaces it.
                Err(err) => warn!(app = %id, error = %err, "skipping unreadable app index row"),
            }
        }
        Ok(apps)
    }

    /// Every indexed app's metadata, sorted by id.
    pub fn list(&self) -> Result<Vec<AppMeta>> {
        Ok(self.apps()?.into_iter().map(|app| app.meta).collect())
    }

    /// The commit the index recorded for an app: `None` when the app is not
    /// indexed, `Some(None)` when it has no git checkout.
    pub fn commit(&self, id: &str) -> Result<Option<Option<String>>> {
        self.ensure_fresh();
        let conn = self.lock();
        Ok(conn
            .query_row("SELECT commit_sha FROM apps WHERE id = ?1", [id], |row| {
                row.get::<_, Option<String>>(0)
            })
            .optional()?)
    }

    /// One file of an app's `.asc/` as the index has it.
    pub fn state_file(&self, id: &str, name: &str) -> Result<StateFile> {
        self.ensure_fresh();
        let conn = self.lock();
        let indexed: bool = conn
            .query_row("SELECT 1 FROM apps WHERE id = ?1", [id], |_| Ok(true))
            .optional()?
            .unwrap_or(false);
        if !indexed {
            return Ok(StateFile::NotIndexed);
        }
        let content: Option<Option<Vec<u8>>> = conn
            .query_row(
                "SELECT content FROM app_files WHERE app_id = ?1 AND name = ?2",
                [id, name],
                |row| row.get(0),
            )
            .optional()?;
        Ok(match content {
            None => StateFile::Absent,
            Some(None) => StateFile::TooLarge,
            Some(Some(bytes)) => StateFile::Content(bytes),
        })
    }

    pub fn status(&self) -> Result<IndexStatus> {
        let conn = self.lock();
        let count = |sql: &str| -> rusqlite::Result<u64> {
            conn.query_row(sql, [], |row| row.get::<_, i64>(0))
                .map(|n| n as u64)
        };
        let apps = count("SELECT count(*) FROM apps")?;
        let files = count("SELECT count(*) FROM app_files")?;
        let number = |key: &str| -> rusqlite::Result<Option<i64>> {
            Ok(schema::get(&conn, key)?.and_then(|v| v.parse().ok()))
        };
        let size = |path: PathBuf| fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        Ok(IndexStatus {
            path: self.path.clone(),
            apps_root: self.root.clone(),
            schema_version: number("version")?.unwrap_or(schema::VERSION),
            apps,
            files,
            size_bytes: size(self.path.clone())
                + size(self.path.with_file_name(format!("{FILE}-wal"))),
            last_full_scan: number("last_full_scan")?,
            last_verify: number("last_verify")?,
            watching: self.watching(),
        })
    }

    // ── Writes ──────────────────────────────────────────────────────────

    /// Re-read one app from disk and replace its rows, or drop them when the
    /// app is gone. The files are read inside the write transaction; the
    /// commit is re-resolved only when the `.git` stamp moved.
    pub fn sync_app(&self, id: &str) -> Result<SyncOutcome> {
        validate_id(id)?;
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stored: Option<(String, Option<String>)> = tx
            .query_row(
                "SELECT git_stamp, commit_sha FROM apps WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let outcome = match scan::read_app(&self.root, id) {
            Ok(snapshot) => {
                let commit = match &stored {
                    Some((stamp, commit)) if *stamp == snapshot.git_stamp => commit.clone(),
                    _ => head_commit(&self.root, id),
                };
                write_app(&tx, &snapshot, commit.as_deref())?;
                self.broken
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(id);
                if stored.is_some() {
                    SyncOutcome::Updated
                } else {
                    SyncOutcome::Added
                }
            }
            Err(ReadError::Missing) => remove_rows(&tx, id)?,
            Err(ReadError::Broken(reason)) => {
                warn!(app = %id, reason, "skipping app with broken meta.json");
                if let Ok(Some(stamps)) = scan::stamps(&self.root.join(id)) {
                    self.broken
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(id.to_string(), stamps);
                }
                remove_rows(&tx, id)?
            }
            Err(ReadError::Io(err)) => {
                return Err(err).with_context(|| format!("cannot read app '{id}' for the index"));
            }
        };
        tx.commit()?;
        Ok(outcome)
    }

    /// Bring the index in line with the files using `stat` only: apps whose
    /// stamps differ are re-synced, vanished ones dropped, new ones added.
    pub fn verify(&self) -> Result<VerifyReport> {
        let started = Instant::now();
        let stored = self.stored_stamps()?;
        let ids = scan::app_ids(&self.root)
            .with_context(|| format!("cannot read apps root {}", self.root.display()))?;
        let mut report = VerifyReport::default();
        let mut seen = HashSet::with_capacity(ids.len());
        for id in ids {
            let current = match scan::stamps(&self.root.join(&id)) {
                Ok(current) => current,
                Err(err) => {
                    warn!(app = %id, error = %err, "cannot stat app files for the index");
                    continue;
                }
            };
            let Some(current) = current else {
                continue;
            };
            report.checked += 1;
            if stored.get(&id) == Some(&current) {
                seen.insert(id);
                continue;
            }
            let known_broken = self
                .broken
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&id)
                == Some(&current);
            if known_broken {
                continue;
            }
            seen.insert(id.clone());
            self.count_sync(&id, &mut report);
        }
        for id in stored.keys().filter(|id| !seen.contains(*id)) {
            self.count_sync(id, &mut report);
        }
        {
            let conn = self.lock();
            schema::set(&conn, "last_verify", &now_ms().to_string())?;
        }
        report.millis = started.elapsed().as_millis() as u64;
        debug!(?report, "app index verified");
        Ok(report)
    }

    fn count_sync(&self, id: &str, report: &mut VerifyReport) {
        match self.sync_app(id) {
            Ok(SyncOutcome::Added) => report.added += 1,
            Ok(SyncOutcome::Updated) => report.updated += 1,
            Ok(SyncOutcome::Removed) => report.removed += 1,
            Ok(SyncOutcome::Unchanged) => {}
            Err(err) => {
                warn!(app = %id, error = %format!("{err:#}"), "cannot sync app into the index")
            }
        }
    }

    /// Re-sync one app only if its files changed since it was indexed —
    /// what the watcher calls, so the events of the daemon's own writes
    /// (already written through) cost a few `stat`s and nothing more.
    pub(crate) fn sync_if_stale(&self, id: &str) -> Result<SyncOutcome> {
        let current = scan::stamps(&self.root.join(id))?;
        let stored = self.stored_stamps_of(id)?;
        if current == stored {
            return Ok(SyncOutcome::Unchanged);
        }
        self.sync_app(id)
    }

    /// Rebuild the whole index from the files in one transaction.
    pub fn rebuild(&self) -> Result<RebuildReport> {
        let started = Instant::now();
        let ids = scan::app_ids(&self.root)
            .with_context(|| format!("cannot read apps root {}", self.root.display()))?;
        let mut report = RebuildReport::default();
        let mut broken = HashMap::new();
        {
            let mut conn = self.lock();
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute("DELETE FROM app_files", [])?;
            tx.execute("DELETE FROM apps", [])?;
            for id in ids {
                match scan::read_app(&self.root, &id) {
                    Ok(snapshot) => {
                        write_app(&tx, &snapshot, head_commit(&self.root, &id).as_deref())?;
                        report.apps += 1;
                    }
                    Err(ReadError::Missing) => {}
                    Err(ReadError::Broken(reason)) => {
                        warn!(app = %id, reason, "skipping app with broken meta.json");
                        if let Ok(Some(stamps)) = scan::stamps(&self.root.join(&id)) {
                            broken.insert(id, stamps);
                        }
                        report.skipped += 1;
                    }
                    Err(ReadError::Io(err)) => {
                        warn!(app = %id, error = %err, "cannot read app for the index");
                        report.skipped += 1;
                    }
                }
            }
            let now = now_ms().to_string();
            schema::set(&tx, "apps_root", &self.root.to_string_lossy())?;
            schema::set(&tx, "last_full_scan", &now)?;
            schema::set(&tx, "last_verify", &now)?;
            tx.commit()?;
        }
        *self.broken.lock().unwrap_or_else(|e| e.into_inner()) = broken;
        report.millis = started.elapsed().as_millis() as u64;
        Ok(report)
    }

    /// Store the runtime states the listing just observed, for the apps
    /// whose state changed (DMN-162 groundwork: the last known state of
    /// every app). Best effort.
    pub fn record_observed(&self, changes: &[(String, &'static str)]) {
        if changes.is_empty() {
            return;
        }
        let result = (|| -> rusqlite::Result<()> {
            let mut conn = self.lock();
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            {
                let mut update = tx.prepare_cached(
                    "UPDATE apps SET observed_state = ?2, observed_at = ?3 WHERE id = ?1",
                )?;
                let now = now_ms();
                for (id, state) in changes {
                    update.execute(params![id, state, now])?;
                }
            }
            tx.commit()
        })();
        if let Err(err) = result {
            debug!(error = %err, "cannot record observed app states");
        }
    }

    fn stored_stamps(&self) -> Result<HashMap<String, AppStamps>> {
        let conn = self.lock();
        let mut stamps: HashMap<String, AppStamps> = HashMap::new();
        {
            let mut stmt = conn.prepare_cached("SELECT id, git_stamp FROM apps")?;
            let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get(1)?)))?;
            for row in rows {
                let (id, git) = row?;
                stamps.entry(id).or_default().git = git;
            }
        }
        let mut stmt = conn.prepare_cached("SELECT app_id, name, stamp FROM app_files")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (id, name, stamp) = row?;
            if let Some(app) = stamps.get_mut(&id) {
                app.files.insert(name, stamp);
            }
        }
        Ok(stamps)
    }

    fn stored_stamps_of(&self, id: &str) -> Result<Option<AppStamps>> {
        let conn = self.lock();
        let Some(git) = conn
            .query_row("SELECT git_stamp FROM apps WHERE id = ?1", [id], |row| {
                row.get::<_, String>(0)
            })
            .optional()?
        else {
            return Ok(None);
        };
        let mut stmt =
            conn.prepare_cached("SELECT name, stamp FROM app_files WHERE app_id = ?1")?;
        let files = stmt
            .query_map([id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(Some(AppStamps { files, git }))
    }
}

fn remove_rows(conn: &Connection, id: &str) -> rusqlite::Result<SyncOutcome> {
    conn.execute("DELETE FROM app_files WHERE app_id = ?1", [id])?;
    let removed = conn.execute("DELETE FROM apps WHERE id = ?1", [id])?;
    Ok(if removed > 0 {
        SyncOutcome::Removed
    } else {
        SyncOutcome::Unchanged
    })
}

// ── Process-wide registry ───────────────────────────────────────────────

/// Open indexes by apps root. `AppStore` is constructed from a bare root in
/// many places; they all find the one index of their root here.
#[derive(Default)]
struct Registry {
    roots: HashMap<PathBuf, Arc<AppIndex>>,
    /// Roots that work without an index in this process: the database could
    /// not be opened, or its file already serves another root here.
    without: HashSet<PathBuf>,
}

fn registry() -> MutexGuard<'static, Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn disabled() -> bool {
    std::env::var_os(DISABLE_ENV).is_some_and(|v| !v.is_empty() && v != "0")
}

/// The index of `config`'s apps directory, opening it on first use.
/// `None` — the process works on the files directly: the index is disabled,
/// its file cannot be opened (a read-only data directory), or the same file
/// already serves another apps directory in this process.
pub fn attach(config: &Config) -> Option<Arc<AppIndex>> {
    attach_with(config, false)
}

/// [`attach`] for the daemon: integrity check, initial verify, and the
/// index is marked as kept current by this process.
pub fn attach_owner(config: &Config) -> Option<Arc<AppIndex>> {
    let index = attach_with(config, true)?;
    index.set_maintained();
    Some(index)
}

fn attach_with(config: &Config, owner: bool) -> Option<Arc<AppIndex>> {
    if disabled() {
        return None;
    }
    let root = normalize(&config.daemon.apps_dir);
    let db = normalize(&config.daemon.data_dir).join(FILE);
    let mut registry = registry();
    if let Some(index) = registry.roots.get(&root) {
        return Some(Arc::clone(index));
    }
    if registry.without.contains(&root) {
        return None;
    }
    if registry.roots.values().any(|index| index.path == db) {
        debug!(db = %db.display(), root = %root.display(), "app index file already serves another apps directory, listing files directly");
        registry.without.insert(root);
        return None;
    }
    match AppIndex::open(&db, &root, owner) {
        Ok(index) => {
            let index = Arc::new(index);
            registry.roots.insert(root, Arc::clone(&index));
            Some(index)
        }
        Err(err) => {
            if owner {
                warn!(db = %db.display(), error = %format!("{err:#}"), "app index unavailable, listing the apps directory instead");
            } else {
                debug!(db = %db.display(), error = %format!("{err:#}"), "app index unavailable, listing the apps directory instead");
            }
            registry.without.insert(root);
            None
        }
    }
}

/// The index already opened for `root` in this process, if any.
pub fn for_root(root: &Path) -> Option<Arc<AppIndex>> {
    let root = normalize(root);
    registry().roots.get(&root).cloned()
}

/// Write-through hook: a file at `path` inside some app's directory was
/// just written. Re-syncs that app in whichever index covers it; a path
/// outside every indexed root is ignored. Never fails the caller — the file
/// is already written, and verify picks up what this could not.
pub fn changed(path: &Path) {
    let path = normalize(path);
    let found = {
        let registry = registry();
        registry.roots.iter().find_map(|(root, index)| {
            let rest = path.strip_prefix(root).ok()?;
            let id = rest.components().next()?.as_os_str().to_str()?.to_string();
            Some((Arc::clone(index), id))
        })
    };
    let Some((index, id)) = found else {
        return;
    };
    if validate_id(&id).is_err() {
        return;
    }
    if let Err(err) = index.sync_app(&id) {
        warn!(app = %id, error = %format!("{err:#}"), "cannot update the app index; the next verify picks the change up");
    }
}

/// Keep the daemon's index current: the inotify watcher (Linux) and a
/// verify every [`VERIFY_INTERVAL`]. Must run inside the tokio runtime.
pub fn start_maintenance(index: Arc<AppIndex>) {
    index.set_maintained();
    #[cfg(target_os = "linux")]
    match watch::spawn(Arc::clone(&index)) {
        Ok(()) => index.watching.store(true, Ordering::Relaxed),
        Err(err) => {
            warn!(error = %format!("{err:#}"), "app index watcher unavailable, relying on the periodic verify")
        }
    }
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(VERIFY_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick fires at once; the open already verified.
        tick.tick().await;
        loop {
            tick.tick().await;
            let index = Arc::clone(&index);
            match tokio::task::spawn_blocking(move || index.verify()).await {
                Ok(Ok(report)) if report.changed() => {
                    info!(
                        added = report.added,
                        updated = report.updated,
                        removed = report.removed,
                        "app index caught up with the files"
                    )
                }
                Ok(Ok(_)) => {}
                Ok(Err(err)) => warn!(error = %format!("{err:#}"), "app index verify failed"),
                Err(err) => warn!(error = %err, "app index verify panicked"),
            }
        }
    });
}

#[cfg(test)]
mod tests;
