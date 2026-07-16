//! Application backups (DMN-009): create, restore and rotate archives of an
//! app's repository/config/data directories, pushed to a named storage
//! (`local` always exists; more via `asc backup storage add`, see
//! [`storage`]). `asc.backup.yaml` at the package repository root excludes
//! paths from the archive; the storages and retention count an app backs up
//! to are chosen per app in `asc app settings` (the `backups` category,
//! stored under the `$backup` reserved key — see
//! [`crate::daemon::pkg::settings::SettingValues::backup_policy`]).
//!
//! Scheduled ("period") backups need a task runner, which does not exist yet
//! (DMN-012 is still planned) — `schedule` is recorded in the policy but not
//! enforced; run backups by hand (`asc backup create <app>`) or from an
//! external cron/systemd timer in the meantime.

pub mod glob;
pub mod storage;

use std::fs;
use std::io;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::daemon::apps::AppStore;
use crate::daemon::apps::meta::AppMeta;
use crate::daemon::config::Config;
use storage::{BackupStorage, StorageList};

/// The three directories a backup covers (`meta.json`, the fourth thing
/// under an app directory, is never included — it is regenerated, not
/// restored, same reasoning as a clone).
const BACKED_UP_DIRS: [&str; 3] = ["repository", "config", "data"];

/// `asc.backup.yaml`, optional, at the repository root: paths to leave out
/// of the archive, relative to the app directory (e.g. `data/cache/**`,
/// `repository/vendor`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupManifest {
    #[serde(default)]
    pub exclude: Vec<String>,
}

impl BackupManifest {
    pub const FILE: &'static str = "asc.backup.yaml";

    /// Load from an app's repository directory; a missing file means no
    /// exclusions (everything is backed up).
    pub fn load(repository_dir: &Path) -> Result<Self> {
        let path = repository_dir.join(Self::FILE);
        match fs::read_to_string(&path) {
            Ok(raw) => {
                serde_yaml::from_str(&raw).with_context(|| format!("invalid {}", path.display()))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }
}

/// What one `create_backup` call produced.
#[derive(Debug)]
pub struct BackupInfo {
    pub name: String,
    pub storage: String,
    pub bytes: u64,
}

/// The storage implementation for `name`: the built-in `local` storage
/// (`<data_dir>/backups`, no configuration needed) or a configured entry.
pub fn resolve_storage(
    config: &Config,
    storages: &StorageList,
    name: &str,
) -> Result<Box<dyn BackupStorage>> {
    if name == storage::LOCAL_NAME {
        return Ok(Box::new(storage::Local {
            dir: config.daemon.data_dir.join("backups"),
        }));
    }
    let entry = storages
        .get(name)
        .with_context(|| format!("backup storage '{name}' not found (asc backup storage list)"))?;
    Ok(storage::open(&entry.kind))
}

/// Archive `meta`'s repository/config/data directories and push them to
/// `storage_name`. `keep` (from the app's backup policy) rotates that
/// storage down to the N most recent backups of this app right after — a
/// failed rotation does not fail the backup itself, it already succeeded.
pub fn create_backup(
    config: &Config,
    store: &AppStore,
    meta: &AppMeta,
    storages: &StorageList,
    storage_name: &str,
    keep: Option<u32>,
) -> Result<BackupInfo> {
    let app_dir = store.app_dir(&meta.id)?;
    let exclude = BackupManifest::load(&app_dir.join("repository"))?.exclude;
    let storage = resolve_storage(config, storages, storage_name)?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let remote_name = format!("{}-{}.tar.gz", meta.id, now.as_secs());
    // Nanosecond-unique on its own — unlike `remote_name` (seconds, by
    // design: it is the retention/sort key), so two concurrent backups
    // never share a local staging path even for the same app in the same
    // second.
    let tmp_archive = std::env::temp_dir().join(format!(
        "asc-backup-{}-{}.tar.gz",
        std::process::id(),
        now.as_nanos()
    ));

    let result = (|| -> Result<u64> {
        {
            let file = fs::File::create(&tmp_archive)
                .with_context(|| format!("cannot create {}", tmp_archive.display()))?;
            let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
                file,
                flate2::Compression::default(),
            ));
            for sub in BACKED_UP_DIRS {
                let dir = app_dir.join(sub);
                if dir.is_dir() {
                    append_tree(&mut builder, &dir, sub, &exclude)?;
                }
            }
            builder
                .into_inner()
                .context("cannot finalize backup archive")?
                .finish()
                .context("cannot finalize backup archive")?;
        }
        let bytes = fs::metadata(&tmp_archive).map(|m| m.len()).unwrap_or(0);
        storage
            .push(&tmp_archive, &remote_name)
            .with_context(|| format!("cannot upload backup to storage '{storage_name}'"))?;
        Ok(bytes)
    })();
    let _ = fs::remove_file(&tmp_archive);
    let bytes = result?;

    if let Some(keep) = keep {
        // Rotation is a courtesy on top of an already-successful backup —
        // its own failure must not turn into an error for the caller.
        let _ = prune(storage.as_ref(), &meta.id, keep);
    }

    Ok(BackupInfo {
        name: remote_name,
        storage: storage_name.to_string(),
        bytes,
    })
}

/// Download `backup_name` from `storage_name` and extract it over `meta`'s
/// app directory — `repository/`, `config/` and `data/` are replaced
/// wholesale (removed, then re-extracted) so the result is exactly the
/// backed-up snapshot, not a merge with whatever was there before. The app
/// should be stopped first; the CLI enforces that.
pub fn restore_backup(
    config: &Config,
    store: &AppStore,
    meta: &AppMeta,
    storages: &StorageList,
    storage_name: &str,
    backup_name: &str,
) -> Result<()> {
    let app_dir = store.app_dir(&meta.id)?;
    let storage = resolve_storage(config, storages, storage_name)?;
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tmp_archive = std::env::temp_dir().join(format!(
        "asc-restore-{}-{unique}.tar.gz",
        std::process::id()
    ));

    let result = (|| -> Result<()> {
        storage
            .pull(backup_name, &tmp_archive)
            .with_context(|| format!("cannot download backup '{backup_name}'"))?;
        for sub in BACKED_UP_DIRS {
            let dir = app_dir.join(sub);
            if dir.exists() {
                fs::remove_dir_all(&dir)
                    .with_context(|| format!("cannot clear {}", dir.display()))?;
            }
        }
        let file = fs::File::open(&tmp_archive)
            .with_context(|| format!("cannot open downloaded backup {}", tmp_archive.display()))?;
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
        archive
            .unpack(&app_dir)
            .with_context(|| format!("cannot extract backup into {}", app_dir.display()))?;
        Ok(())
    })();
    let _ = fs::remove_file(&tmp_archive);
    result
}

/// Backups of `app_id` on `storage_name`, oldest first.
pub fn list_backups(
    config: &Config,
    storages: &StorageList,
    storage_name: &str,
    app_id: &str,
) -> Result<Vec<String>> {
    resolve_storage(config, storages, storage_name)?.list(app_id)
}

/// Delete the oldest backups of `app_id` beyond `keep` (DMN-009 rotation).
/// Best-effort per file: one failed deletion does not stop the rest.
pub fn prune(storage: &dyn BackupStorage, app_id: &str, keep: u32) -> Result<Vec<String>> {
    let names = storage.list(app_id)?;
    let keep = keep as usize;
    let mut removed = Vec::new();
    if names.len() > keep {
        for name in &names[..names.len() - keep] {
            if storage.remove(name).is_ok() {
                removed.push(name.clone());
            }
        }
    }
    Ok(removed)
}

/// Add every file under `dir` to `builder` as `<rel_prefix>/...`, skipping
/// symlinks (never followed, never recreated — same rule as
/// [`crate::daemon::apps::disk::dir_size`]) and anything [`glob::matches_any`]
/// excludes.
fn append_tree(
    builder: &mut tar::Builder<impl io::Write>,
    dir: &Path,
    rel_prefix: &str,
    exclude: &[String],
) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let rel = format!("{rel_prefix}/{}", entry.file_name().to_string_lossy());
        if glob::matches_any(exclude, &rel) {
            continue;
        }
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            append_tree(builder, &path, &rel, exclude)?;
        } else {
            let mut file =
                fs::File::open(&path).with_context(|| format!("cannot read {}", path.display()))?;
            builder
                .append_file(&rel, &mut file)
                .with_context(|| format!("cannot archive {}", path.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::apps::meta::{DesiredState, Owner, Runtime};

    fn seed_app(store: &AppStore, id: &str, exclude: &[&str]) -> AppMeta {
        let app_dir = store.app_dir(id).unwrap();
        fs::create_dir_all(app_dir.join("repository")).unwrap();
        fs::write(app_dir.join("repository/asc.yaml"), "name: demo\n").unwrap();
        if !exclude.is_empty() {
            let yaml = format!(
                "exclude:\n{}\n",
                exclude
                    .iter()
                    .map(|p| format!("  - \"{p}\""))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            fs::write(app_dir.join("repository/asc.backup.yaml"), yaml).unwrap();
        }
        fs::create_dir_all(app_dir.join("config")).unwrap();
        fs::create_dir_all(app_dir.join("data/cache")).unwrap();
        fs::write(app_dir.join("data/save.txt"), b"progress=1").unwrap();
        fs::write(app_dir.join("data/cache/tmp.bin"), b"throwaway").unwrap();

        let meta = AppMeta {
            id: id.to_string(),
            name: "Demo".into(),
            custom_name: None,
            owner: Owner {
                uid: 1000,
                name: "tester".into(),
            },
            version: Some("1.0.0".into()),
            source: None,
            package: None,
            desired_state: DesiredState::Stopped,
            quota: None,
            runtime: Runtime::Process {
                command: "/bin/sh".into(),
                args: vec![],
            },
        };
        store.save(&meta).unwrap();
        meta
    }

    #[test]
    fn create_excludes_and_restore_roundtrips() {
        let ws = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.daemon.data_dir = ws.path().join("data");
        config.daemon.apps_dir = ws.path().join("apps");
        let store = AppStore::new(config.daemon.apps_dir.clone());
        let meta = seed_app(&store, "demo", &["data/cache/**"]);
        let storages = StorageList::load_with(crate::daemon::pkg::sources::Scope::User).unwrap();

        let info =
            create_backup(&config, &store, &meta, &storages, storage::LOCAL_NAME, None).unwrap();
        assert!(info.name.starts_with("demo-") && info.name.ends_with(".tar.gz"));
        assert!(info.bytes > 0);

        // Wipe the app directory's data, then restore — the excluded cache
        // file must not come back, but save.txt must.
        fs::remove_dir_all(store.app_dir("demo").unwrap().join("data")).unwrap();
        restore_backup(
            &config,
            &store,
            &meta,
            &storages,
            storage::LOCAL_NAME,
            &info.name,
        )
        .unwrap();
        let app_dir = store.app_dir("demo").unwrap();
        assert_eq!(
            fs::read_to_string(app_dir.join("data/save.txt")).unwrap(),
            "progress=1"
        );
        assert!(!app_dir.join("data/cache/tmp.bin").exists());
        assert!(app_dir.join("repository/asc.yaml").exists());
    }

    #[test]
    fn prune_keeps_only_the_newest() {
        let ws = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.daemon.data_dir = ws.path().join("data");
        config.daemon.apps_dir = ws.path().join("apps");
        let store = AppStore::new(config.daemon.apps_dir.clone());
        let meta = seed_app(&store, "demo", &[]);
        let storages = StorageList::load_with(crate::daemon::pkg::sources::Scope::User).unwrap();

        let mut names = Vec::new();
        for _ in 0..3 {
            let info = create_backup(&config, &store, &meta, &storages, storage::LOCAL_NAME, None)
                .unwrap();
            names.push(info.name);
            // Backup names are second-resolution timestamps; force distinct
            // ones so pruning has a real oldest/newest to pick between.
            std::thread::sleep(std::time::Duration::from_millis(1100));
        }

        let storage = resolve_storage(&config, &storages, storage::LOCAL_NAME).unwrap();
        let removed = prune(storage.as_ref(), "demo", 1).unwrap();
        assert_eq!(removed.len(), 2);
        let remaining = storage.list("demo").unwrap();
        assert_eq!(remaining, vec![names[2].clone()]);
    }
}
