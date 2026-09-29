//! On-disk layout of one app directory (DMN-139).
//!
//! ```text
//! /asc/apps/<id>/
//! ├── .asc/            # daemon-owned state, hidden from a plain `ls`
//! │   ├── meta.json    # AppMeta — the source of truth for recovery
//! │   └── settings.json # chosen setting values (SettingValues)
//! ├── repository/
//! └── data/
//! ```
//!
//! Before DMN-139 `meta.json` sat at the app directory root and the setting
//! values in `config/settings.json`. [`migrate`] moves an app laid out that
//! way into `.asc/` the first time the store reads it; until that succeeds
//! (a regular user's CLI reading a tree it cannot write) the readers fall
//! back to the legacy paths, so the app never disappears from the listing.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::meta::AppMeta;
use crate::daemon::pkg::settings::SettingValues;

/// Hidden directory with the daemon's own files inside an app directory.
pub const STATE_DIR: &str = ".asc";

/// Pre-DMN-139 home of `settings.json`.
const LEGACY_CONFIG_DIR: &str = "config";

/// `<app_dir>/.asc`.
pub fn state_dir(app_dir: &Path) -> PathBuf {
    app_dir.join(STATE_DIR)
}

/// Pre-DMN-139 `meta.json` at the app directory root.
pub fn legacy_meta_path(app_dir: &Path) -> PathBuf {
    app_dir.join(AppMeta::FILE)
}

/// Directory holding the app's `settings.json`: `.asc/`, or the legacy
/// `config/` while an unmigrated app still keeps its values there.
pub fn settings_dir(app_dir: &Path) -> PathBuf {
    let state = state_dir(app_dir);
    let legacy = app_dir.join(LEGACY_CONFIG_DIR);
    if !state.join(SettingValues::FILE).exists() && legacy.join(SettingValues::FILE).exists() {
        legacy
    } else {
        state
    }
}

/// Create `.asc/` if missing, owned like the app directory itself: the root
/// daemon writing into a regular user's `~/.asc/apps/<id>` must not leave a
/// root-owned directory that user's own CLI can no longer write to.
pub fn ensure_state_dir(app_dir: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::MetadataExt;

    let state = state_dir(app_dir);
    match fs::create_dir(&state) {
        Ok(()) => {
            if let Ok(md) = fs::metadata(app_dir) {
                // Best effort: a non-root caller only ever creates it as
                // itself, which is already the right owner for its own tree.
                let _ = std::os::unix::fs::chown(&state, Some(md.uid()), Some(md.gid()));
            }
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => {
            return Err(e).with_context(|| format!("cannot create {}", state.display()));
        }
    }
    Ok(state)
}

/// Move a pre-DMN-139 app (`meta.json` at the root, `config/settings.json`)
/// into `.asc/`. Idempotent; `Ok(false)` when there was nothing to move.
///
/// Settings go first and meta last: meta at its legacy path is what marks
/// the app as not yet migrated, so an interrupted run is simply redone.
pub fn migrate(app_dir: &Path) -> Result<bool> {
    let legacy_meta = legacy_meta_path(app_dir);
    let state = state_dir(app_dir);
    if !legacy_meta.exists() || state.join(AppMeta::FILE).exists() {
        return Ok(false);
    }
    ensure_state_dir(app_dir)?;

    let config = app_dir.join(LEGACY_CONFIG_DIR);
    let legacy_settings = config.join(SettingValues::FILE);
    let settings = state.join(SettingValues::FILE);
    if legacy_settings.exists() && !settings.exists() {
        fs::rename(&legacy_settings, &settings)
            .with_context(|| format!("cannot move {}", legacy_settings.display()))?;
    }
    // Leftovers of an interrupted atomic save; then drop config/ if that
    // leaves it empty — anything else in there is not ours to delete.
    let _ = fs::remove_file(config.join("settings.json.tmp"));
    let _ = fs::remove_dir(&config);
    let _ = fs::remove_file(app_dir.join("meta.json.tmp"));

    fs::rename(&legacy_meta, state.join(AppMeta::FILE))
        .with_context(|| format!("cannot move {}", legacy_meta.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_app(dir: &Path) {
        fs::write(dir.join("meta.json"), b"{\"meta\":1}").unwrap();
        fs::create_dir_all(dir.join("config")).unwrap();
        fs::write(dir.join("config/settings.json"), b"{\"s\":1}").unwrap();
    }

    #[test]
    fn migrates_legacy_layout() {
        let dir = tempfile::tempdir().unwrap();
        legacy_app(dir.path());
        assert_eq!(settings_dir(dir.path()), dir.path().join("config"));

        assert!(migrate(dir.path()).unwrap());
        assert_eq!(
            fs::read(dir.path().join(".asc/meta.json")).unwrap(),
            b"{\"meta\":1}"
        );
        assert_eq!(
            fs::read(dir.path().join(".asc/settings.json")).unwrap(),
            b"{\"s\":1}"
        );
        assert!(!dir.path().join("meta.json").exists());
        assert!(!dir.path().join("config").exists());
        assert_eq!(settings_dir(dir.path()), dir.path().join(".asc"));

        // Idempotent.
        assert!(!migrate(dir.path()).unwrap());
    }

    #[test]
    fn nothing_to_migrate_on_fresh_or_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!migrate(dir.path()).unwrap());
        assert!(!dir.path().join(".asc").exists());
    }

    #[test]
    fn resumes_interrupted_migration() {
        // Settings already moved, meta still at the root.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("meta.json"), b"{}").unwrap();
        fs::create_dir_all(dir.path().join(".asc")).unwrap();
        fs::write(dir.path().join(".asc/settings.json"), b"new").unwrap();
        fs::create_dir_all(dir.path().join("config")).unwrap();

        assert!(migrate(dir.path()).unwrap());
        assert!(dir.path().join(".asc/meta.json").exists());
        assert_eq!(
            fs::read(dir.path().join(".asc/settings.json")).unwrap(),
            b"new"
        );
        assert!(!dir.path().join("config").exists());
    }

    #[test]
    fn keeps_foreign_files_in_config() {
        let dir = tempfile::tempdir().unwrap();
        legacy_app(dir.path());
        fs::write(dir.path().join("config/notes.txt"), b"mine").unwrap();

        assert!(migrate(dir.path()).unwrap());
        assert!(dir.path().join("config/notes.txt").exists());
        assert!(dir.path().join(".asc/settings.json").exists());
    }
}
