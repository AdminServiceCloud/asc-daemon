//! App storage: `/asc/apps/<id>/` directories with `meta.json` inside.
//!
//! The store is the on-disk index: listing scans the apps root and reads each
//! meta.json. Broken entries are skipped with a warning instead of failing
//! the whole listing — one corrupted app must not hide the others.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::warn;

use super::meta::{AppMeta, validate_id};

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

    /// Load one app's metadata; `None` when it is not installed.
    pub fn get(&self, id: &str) -> Result<Option<AppMeta>> {
        let dir = self.app_dir(id)?;
        if !dir.join(AppMeta::FILE).exists() {
            return Ok(None);
        }
        AppMeta::load(&dir).map(Some)
    }

    /// Persist an app's metadata, creating its directory if needed.
    pub fn save(&self, meta: &AppMeta) -> Result<()> {
        let dir = self.app_dir(&meta.id)?;
        fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create app directory {}", dir.display()))?;
        meta.save(&dir)
    }

    /// Remove an app's directory with all its data.
    pub fn remove(&self, id: &str) -> Result<()> {
        let dir = self.app_dir(id)?;
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => {
                Err(e).with_context(|| format!("cannot remove app directory {}", dir.display()))
            }
        }
    }

    /// All installed apps, sorted by id. Invalid entries are skipped.
    pub fn list(&self) -> Result<Vec<AppMeta>> {
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
            if !dir.is_dir() || !dir.join(AppMeta::FILE).exists() {
                continue;
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::apps::meta::{DesiredState, Owner, Runtime};

    fn meta(id: &str, uid: u32) -> AppMeta {
        AppMeta {
            id: id.into(),
            name: id.into(),
            custom_name: None,
            owner: Owner {
                uid,
                name: format!("user{uid}"),
            },
            version: None,
            source: None,
            package: None,
            desired_state: DesiredState::Stopped,
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
        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join(AppMeta::FILE), "{ not json").unwrap();

        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "good");
    }

    #[test]
    fn rejects_traversal_ids() {
        let dir = tempfile::tempdir().unwrap();
        let store = AppStore::new(dir.path());
        assert!(store.get("../oops").is_err());
        assert!(store.app_dir("a/b").is_err());
    }
}
