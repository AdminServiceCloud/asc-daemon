//! App storage: `/asc/apps/<id>/` directories with `.asc/meta.json` inside.
//!
//! The files are the source of truth; listings come from the app index
//! (the daemon's local database, [`super::index`]) when one is open for this
//! apps root, and from a scan of the root otherwise. Every write goes to the
//! file first and is then written through into the index.
//!
//! The scan reads each meta.json, first moving a legacy app into `.asc/`
//! ([`layout::migrate`]). Broken entries are skipped with a warning instead
//! of failing the whole listing — one corrupted app must not hide the
//! others.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::{info, warn};

use super::index::{self, AppIndex, IndexedApp, StateFile};
use super::layout;
use super::meta::{AppMeta, validate_id};
use crate::daemon::pkg::settings::SettingValues;

pub struct AppStore {
    root: PathBuf,
}

impl AppStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory of an app (validates the id first).
    pub fn app_dir(&self, id: &str) -> Result<PathBuf> {
        validate_id(id)?;
        Ok(self.root.join(id))
    }

    /// The index open for this apps root in this process, if any.
    pub fn index(&self) -> Option<Arc<AppIndex>> {
        index::for_root(&self.root)
    }

    /// Write one app through into the index after its files changed.
    fn sync(&self, id: &str) {
        if let Some(index) = self.index()
            && let Err(err) = index.sync_app(id)
        {
            warn!(app = %id, error = %format!("{err:#}"), "cannot update the app index; the next verify picks the change up");
        }
    }

    /// Load one app's metadata; `None` when it is not installed. Always read
    /// from the file: operating on an app must see exactly what is on disk.
    pub fn get(&self, id: &str) -> Result<Option<AppMeta>> {
        let dir = self.app_dir(id)?;
        if !AppMeta::exists(&dir) {
            return Ok(None);
        }
        migrate(&dir);
        AppMeta::load(&dir).map(Some)
    }

    /// Persist an app's metadata, creating its directory if needed.
    pub fn save(&self, meta: &AppMeta) -> Result<()> {
        let dir = self.app_dir(&meta.id)?;
        fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create app directory {}", dir.display()))?;
        meta.save(&dir)?;
        self.sync(&meta.id);
        Ok(())
    }

    /// Remove an app's directory with all its data.
    pub fn remove(&self, id: &str) -> Result<()> {
        let dir = self.app_dir(id)?;
        match fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("cannot remove app directory {}", dir.display()));
            }
        }
        self.sync(id);
        Ok(())
    }

    /// All installed apps, sorted by id: from the index when there is one,
    /// else a scan of the apps root.
    pub fn list(&self) -> Result<Vec<AppMeta>> {
        match self.indexed()? {
            Some(apps) => Ok(apps.into_iter().map(|app| app.meta).collect()),
            None => self.scan(),
        }
    }

    /// All installed apps with their indexed commit and last observed state;
    /// `None` when no index serves this root (or it failed — then the caller
    /// scans, the files are the source of truth anyway).
    pub fn indexed(&self) -> Result<Option<Vec<IndexedApp>>> {
        let Some(index) = self.index() else {
            return Ok(None);
        };
        match index.apps() {
            Ok(apps) => Ok(Some(apps)),
            Err(err) => {
                warn!(error = %format!("{err:#}"), "app index unavailable, listing the apps directory");
                Ok(None)
            }
        }
    }

    /// The app's chosen setting values: from the index's copy of its
    /// `settings.json` when it has one, else from the file.
    pub fn settings(&self, id: &str) -> Result<SettingValues> {
        let dir = self.app_dir(id)?;
        if let Some(index) = self.index() {
            let named = layout::state_dir(&dir).join(SettingValues::FILE);
            match index.state_file(id, SettingValues::FILE) {
                Ok(StateFile::Content(raw)) => return SettingValues::parse(&raw, &named),
                Ok(StateFile::Absent) => return Ok(SettingValues::default()),
                Ok(StateFile::NotIndexed | StateFile::TooLarge) => {}
                Err(err) => {
                    warn!(app = %id, error = %format!("{err:#}"), "cannot read settings from the app index")
                }
            }
        }
        SettingValues::load(&layout::settings_dir(&dir))
    }

    /// Scan the apps root directly, bypassing the index. Invalid entries
    /// are skipped.
    pub fn scan(&self) -> Result<Vec<AppMeta>> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            // No apps root yet — nothing installed.
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("cannot read apps root {}", self.root.display()));
            }
        };
        let mut apps = Vec::new();
        for entry in entries {
            let entry = entry?;
            let dir = entry.path();
            if !dir.is_dir() || !AppMeta::exists(&dir) {
                continue;
            }
            migrate(&dir);
            match AppMeta::load(&dir) {
                Ok(meta) => {
                    if meta.id != entry.file_name().to_string_lossy() {
                        warn!(dir = %dir.display(), id = %meta.id, "meta.json id does not match directory name, skipping");
                        continue;
                    }
                    apps.push(meta);
                }
                Err(err) => {
                    warn!(dir = %dir.display(), error = %format!("{err:#}"), "skipping app with broken meta.json")
                }
            }
        }
        apps.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(apps)
    }
}

/// Move a legacy app into `.asc/`. A failure is not fatal: the readers
/// fall back to the legacy paths, so the app stays listed and usable.
pub(crate) fn migrate(dir: &Path) {
    match layout::migrate(dir) {
        Ok(true) => info!(dir = %dir.display(), "moved app metadata and settings into .asc/"),
        Ok(false) => {}
        Err(err) => {
            warn!(dir = %dir.display(), error = %format!("{err:#}"), "cannot move app metadata into .asc/, using the legacy layout")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::apps::meta::{DesiredState, Owner, Runtime};

    fn meta(id: &str, uid: u32) -> AppMeta {
        AppMeta {
            id: id.into(),
            uuid: None,
            name: id.into(),
            custom_name: None,
            owner: Owner {
                uid,
                name: format!("user{uid}"),
            },
            version: None,
            source: None,
            branch: None,
            repo_path: None,
            package: None,
            install_method: None,
            desired_state: DesiredState::Stopped,
            setup_skipped: false,
            quota: None,
            runtime: Runtime::Process {
                command: "true".into(),
                args: vec![],
            },
        }
    }

    #[test]
    fn empty_root_lists_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = AppStore::new(dir.path().join("apps"));
        assert!(store.list().unwrap().is_empty());
        assert!(store.get("missing").unwrap().is_none());
    }

    #[test]
    fn save_get_list_remove() {
        let dir = tempfile::tempdir().unwrap();
        let store = AppStore::new(dir.path().join("apps"));
        store.save(&meta("bbb", 1000)).unwrap();
        store.save(&meta("aaa", 1001)).unwrap();

        let listed = store.list().unwrap();
        assert_eq!(
            listed.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["aaa", "bbb"]
        );
        assert!(store.get("aaa").unwrap().is_some());

        store.remove("aaa").unwrap();
        assert!(store.get("aaa").unwrap().is_none());
        store.remove("aaa").unwrap(); // idempotent
    }

    #[test]
    fn broken_meta_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let store = AppStore::new(dir.path());
        store.save(&meta("good", 1000)).unwrap();
        let bad = dir.path().join("bad");
        fs::create_dir_all(bad.join(".asc")).unwrap();
        fs::write(AppMeta::path(&bad), "{ not json").unwrap();

        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "good");
    }

    #[test]
    fn legacy_layout_is_migrated_on_read() {
        let dir = tempfile::tempdir().unwrap();
        let store = AppStore::new(dir.path());
        let app = dir.path().join("old");
        fs::create_dir_all(app.join("config")).unwrap();
        fs::write(
            app.join("meta.json"),
            serde_json::to_string(&meta("old", 1000)).unwrap(),
        )
        .unwrap();
        fs::write(app.join("config/settings.json"), "{}").unwrap();

        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert!(AppMeta::path(&app).exists());
        assert!(app.join(".asc/settings.json").exists());
        assert!(!app.join("meta.json").exists());
        assert!(!app.join("config").exists());
        assert!(store.get("old").unwrap().is_some());
    }

    #[test]
    fn rejects_traversal_ids() {
        let dir = tempfile::tempdir().unwrap();
        let store = AppStore::new(dir.path());
        assert!(store.get("../oops").is_err());
        assert!(store.app_dir("a/b").is_err());
    }
}
