//! Application backups (DMN-009): create, restore and rotate archives of an
//! app's repository/config/data directories, pushed to a named storage
//! (`local` always exists; more via `asc backup storage add`, see
//! [`storage`]). `asc.backup.yaml` at the package repository root excludes
//! paths from the archive; the storages and retention count an app backs up
//! to are chosen per app in `asc app settings` (the `backups` category,
//! stored under the `$backup` reserved key — see
//! [`crate::daemon::pkg::settings::SettingValues::backup_policy`]).
//!
//! Scheduled backups run inside the daemon: the scheduler (DMN-012,
//! [`crate::daemon::scheduler`]) evaluates each app's policy `schedule`
//! (`daily@HH:MM` or a cron expression) once a minute and calls
//! [`create_backup`] when it fires.

pub mod glob;
pub mod s3;
pub mod storage;

use std::fs;
use std::io;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::daemon::apps::AppStore;
use crate::daemon::apps::meta::AppMeta;
use crate::daemon::config::Config;
use storage::{BackupObject, BackupStorage, StorageList};

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

/// Per-run file selection chosen by the caller (DMN-118), on top of the
/// repository's own `asc.backup.yaml` exclusions. Patterns use the same
/// [`glob`] syntax and are relative to the app directory (`data/**/*.db`,
/// `repository/vendor`). A non-empty `include` narrows the archive to the
/// files it matches (a directory pattern takes everything under it);
/// `exclude` then removes from whatever is left — exclusion always wins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackupFilter {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

impl BackupFilter {
    /// Most patterns a single filter list may carry — a sanity cap on an
    /// API caller, far above what anyone types by hand.
    pub const MAX_PATTERNS: usize = 64;
    const MAX_PATTERN_LEN: usize = 512;

    /// Validate and normalize caller-supplied patterns: trimmed, blank lines
    /// dropped, a trailing `/` stripped. Absolute paths, `..` segments and
    /// backslashes are refused — the patterns only ever match paths inside
    /// the app directory, and a pattern that cannot match anything there is
    /// a mistake worth reporting, not silently ignoring.
    pub fn new(include: Vec<String>, exclude: Vec<String>) -> Result<Self> {
        Ok(Self {
            include: normalize_patterns("include", include)?,
            exclude: normalize_patterns("exclude", exclude)?,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }
}

fn normalize_patterns(kind: &str, raw: Vec<String>) -> Result<Vec<String>> {
    let patterns: Vec<String> = raw
        .into_iter()
        .map(|p| p.trim().trim_end_matches('/').to_string())
        .filter(|p| !p.is_empty())
        .collect();
    if patterns.len() > BackupFilter::MAX_PATTERNS {
        bail!(
            "invalid {kind} patterns: at most {} are allowed",
            BackupFilter::MAX_PATTERNS
        );
    }
    for p in &patterns {
        if p.len() > BackupFilter::MAX_PATTERN_LEN {
            bail!(
                "invalid {kind} pattern '{p}': longer than {} characters",
                BackupFilter::MAX_PATTERN_LEN
            );
        }
        if p.starts_with('/') || p.contains('\\') || p.split('/').any(|seg| seg == "..") {
            bail!("invalid {kind} pattern '{p}': use a path relative to the app directory");
        }
    }
    Ok(patterns)
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
    storage::open(&entry.kind)
}

/// Refuse a backup name that is not one of `app_id`'s archives: the name
/// comes from an API caller and is joined onto a directory (local storage)
/// or an object key (S3), so `../../etc/shadow` or another app's archive
/// must never get that far.
pub fn validate_backup_name(app_id: &str, name: &str) -> Result<()> {
    if name.contains('/') || name.contains('\\') || !storage::belongs_to(name, app_id) {
        bail!("'{name}' is not a backup of app '{app_id}'");
    }
    Ok(())
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
    create_backup_multi(
        config,
        store,
        meta,
        storages,
        &[storage_name.to_string()],
        keep,
        &BackupFilter::default(),
    )
    .pop()
    .map(|(_, result)| result)
    .unwrap_or_else(|| Err(anyhow::anyhow!("no storage given")))
}

/// [`create_backup`] to several storages from **one** archive: the app is
/// archived once, the same file pushed to every storage in turn — both
/// faster and consistent (every copy is the same snapshot). Results come
/// back per storage, in order; one failed upload does not stop the others.
/// `filter` narrows what goes into the archive (see [`BackupFilter`]).
pub fn create_backup_multi(
    config: &Config,
    store: &AppStore,
    meta: &AppMeta,
    storages: &StorageList,
    storage_names: &[String],
    keep: Option<u32>,
    filter: &BackupFilter,
) -> Vec<(String, Result<BackupInfo>)> {
    let fail_all = |err: anyhow::Error| {
        let message = format!("{err:#}");
        storage_names
            .iter()
            .map(|name| (name.clone(), Err(anyhow::anyhow!(message.clone()))))
            .collect::<Vec<_>>()
    };
    let app_dir = match store.app_dir(&meta.id) {
        Ok(dir) => dir,
        Err(err) => return fail_all(err),
    };
    let mut exclude = match BackupManifest::load(&app_dir.join("repository")) {
        Ok(manifest) => manifest.exclude,
        Err(err) => return fail_all(err),
    };
    exclude.extend(filter.exclude.iter().cloned());
    let include = &filter.include;

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

    let built = (|| -> Result<u64> {
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
                    append_tree(&mut builder, &dir, sub, include, &exclude)?;
                }
            }
            builder
                .into_inner()
                .context("cannot finalize backup archive")?
                .finish()
                .context("cannot finalize backup archive")?;
        }
        Ok(fs::metadata(&tmp_archive).map(|m| m.len()).unwrap_or(0))
    })();
    let bytes = match built {
        Ok(bytes) => bytes,
        Err(err) => {
            let _ = fs::remove_file(&tmp_archive);
            return fail_all(err);
        }
    };

    let mut results = Vec::with_capacity(storage_names.len());
    for storage_name in storage_names {
        let pushed = (|| -> Result<BackupInfo> {
            let storage = resolve_storage(config, storages, storage_name)?;
            storage
                .push(&tmp_archive, &remote_name)
                .with_context(|| format!("cannot upload backup to storage '{storage_name}'"))?;
            if let Some(keep) = keep {
                // Rotation is a courtesy on top of an already-successful
                // backup — its own failure must not turn into an error.
                let _ = prune(storage.as_ref(), &meta.id, keep);
            }
            Ok(BackupInfo {
                name: remote_name.clone(),
                storage: storage_name.clone(),
                bytes,
            })
        })();
        results.push((storage_name.clone(), pushed));
    }
    let _ = fs::remove_file(&tmp_archive);
    results
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
    validate_backup_name(&meta.id, backup_name)?;
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
) -> Result<Vec<BackupObject>> {
    resolve_storage(config, storages, storage_name)?.list(app_id)
}

/// Delete one archive of `app_id` from `storage_name`.
pub fn delete_backup(
    config: &Config,
    storages: &StorageList,
    storage_name: &str,
    app_id: &str,
    backup_name: &str,
) -> Result<()> {
    validate_backup_name(app_id, backup_name)?;
    resolve_storage(config, storages, storage_name)?.remove(backup_name)
}

/// Delete the oldest backups of `app_id` beyond `keep` (DMN-009 rotation).
/// Best-effort per file: one failed deletion does not stop the rest.
pub fn prune(storage: &dyn BackupStorage, app_id: &str, keep: u32) -> Result<Vec<String>> {
    let objects = storage.list(app_id)?;
    let keep = keep as usize;
    let mut removed = Vec::new();
    if objects.len() > keep {
        for object in &objects[..objects.len() - keep] {
            if storage.remove(&object.name).is_ok() {
                removed.push(object.name.clone());
            }
        }
    }
    Ok(removed)
}

/// Add every file under `dir` to `builder` as `<rel_prefix>/...`, skipping
/// symlinks (never followed, never recreated — same rule as
/// [`crate::daemon::apps::disk::dir_size`]) and anything [`glob::matches_any`]
/// excludes. With a non-empty `include`, a file goes in only when it (or
/// one of its directories) matches one of those patterns; directories are
/// still walked, since a file deep inside may match.
fn append_tree(
    builder: &mut tar::Builder<impl io::Write>,
    dir: &Path,
    rel_prefix: &str,
    include: &[String],
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
            append_tree(builder, &path, &rel, include, exclude)?;
        } else if include.is_empty() || glob::matches_any(include, &rel) {
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
            uuid: None,
            name: "Demo".into(),
            custom_name: None,
            owner: Owner {
                uid: 1000,
                name: "tester".into(),
            },
            version: Some("1.0.0".into()),
            source: None,
            branch: None,
            repo_path: None,
            package: None,
            install_method: None,
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

    /// Archive paths of the one backup `filter` produces for a seeded app.
    fn archived_paths(filter: &BackupFilter) -> Vec<String> {
        let ws = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.daemon.data_dir = ws.path().join("data");
        config.daemon.apps_dir = ws.path().join("apps");
        let store = AppStore::new(config.daemon.apps_dir.clone());
        let meta = seed_app(&store, "demo", &[]);
        let storages = StorageList::load_with(crate::daemon::pkg::sources::Scope::User).unwrap();
        let names = vec![storage::LOCAL_NAME.to_string()];
        let (_, result) =
            create_backup_multi(&config, &store, &meta, &storages, &names, None, filter)
                .pop()
                .unwrap();
        let info = result.unwrap();
        let archive = config.daemon.data_dir.join("backups").join(&info.name);
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(
            fs::File::open(archive).unwrap(),
        ));
        let mut paths: Vec<String> = tar
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect();
        paths.sort();
        paths
    }

    #[test]
    fn filter_include_narrows_and_exclude_wins() {
        let everything = archived_paths(&BackupFilter::default());
        assert_eq!(
            everything,
            vec!["data/cache/tmp.bin", "data/save.txt", "repository/asc.yaml"]
        );

        let only_data = BackupFilter::new(vec!["data".into()], vec![]).unwrap();
        assert_eq!(
            archived_paths(&only_data),
            vec!["data/cache/tmp.bin", "data/save.txt"]
        );

        let data_without_cache =
            BackupFilter::new(vec!["data/".into()], vec!["data/cache/**".into()]).unwrap();
        assert_eq!(archived_paths(&data_without_cache), vec!["data/save.txt"]);

        let by_extension = BackupFilter::new(vec!["**/*.txt".into()], vec![]).unwrap();
        assert_eq!(archived_paths(&by_extension), vec!["data/save.txt"]);
    }

    #[test]
    fn filter_rejects_patterns_outside_the_app() {
        for bad in ["/etc/passwd", "../other", "data/../../x", "data\\x"] {
            assert!(
                BackupFilter::new(vec![bad.into()], vec![]).is_err(),
                "{bad}"
            );
            assert!(
                BackupFilter::new(vec![], vec![bad.into()]).is_err(),
                "{bad}"
            );
        }
        let filter = BackupFilter::new(vec!["  ".into(), " data/ ".into()], vec![]).unwrap();
        assert_eq!(filter.include, vec!["data"]);
        assert!(
            BackupFilter::new(vec![], vec!["".into()])
                .unwrap()
                .is_empty()
        );
        let too_many = vec!["data".to_string(); BackupFilter::MAX_PATTERNS + 1];
        assert!(BackupFilter::new(too_many, vec![]).is_err());
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
        let remaining: Vec<String> = storage
            .list("demo")
            .unwrap()
            .into_iter()
            .map(|o| o.name)
            .collect();
        assert_eq!(remaining, vec![names[2].clone()]);
    }

    #[test]
    fn multi_storage_backup_and_name_validation() {
        let ws = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.daemon.data_dir = ws.path().join("data");
        config.daemon.apps_dir = ws.path().join("apps");
        let store = AppStore::new(config.daemon.apps_dir.clone());
        let meta = seed_app(&store, "demo", &[]);
        let mut storages =
            StorageList::load_with(crate::daemon::pkg::sources::Scope::User).unwrap();
        storages
            .upsert(storage::StorageEntry {
                name: "second".into(),
                managed_by: Some("platform".into()),
                kind: storage::StorageKind::Local {
                    dir: ws.path().join("second"),
                },
            })
            .unwrap();

        let names = vec![
            storage::LOCAL_NAME.to_string(),
            "second".to_string(),
            "missing".to_string(),
        ];
        let results = create_backup_multi(
            &config,
            &store,
            &meta,
            &storages,
            &names,
            None,
            &BackupFilter::default(),
        );
        assert_eq!(results.len(), 3);
        let first = results[0].1.as_ref().unwrap();
        let second = results[1].1.as_ref().unwrap();
        assert_eq!(
            first.name, second.name,
            "one snapshot, same name everywhere"
        );
        assert!(results[2].1.is_err());

        let listed = list_backups(&config, &storages, "second", "demo").unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].size > 0);

        for bad in [
            "../../etc/shadow",
            "other-1.tar.gz",
            "demo-1/x.tar.gz",
            "demo-abc.tar.gz",
        ] {
            assert!(validate_backup_name("demo", bad).is_err(), "{bad}");
            assert!(delete_backup(&config, &storages, "second", "demo", bad).is_err());
        }
        delete_backup(&config, &storages, "second", "demo", &second.name).unwrap();
        assert!(
            list_backups(&config, &storages, "second", "demo")
                .unwrap()
                .is_empty()
        );
    }
}
