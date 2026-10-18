//! Reading one app's state from disk for the index: the files of its
//! `.asc/` directory with their stamps, and the stamp of its repository's
//! `.git`. Two depths: [`stamps`] only `stat`s (what verify runs every
//! minute), [`read_app`] also reads the contents (what a sync stores).

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::daemon::apps::layout;
use crate::daemon::apps::meta::{AppMeta, validate_id};
use crate::daemon::pkg::settings::SettingValues;

/// Files above this size are indexed without their content: `.asc/` holds
/// small JSON/YAML documents, and a stray large file there must not bloat
/// the database. `meta.json` is always read in full — the index cannot
/// describe an app without it.
pub const MAX_MIRRORED_BYTES: u64 = 1024 * 1024;

/// One indexed file: its logical name inside `.asc/`, the content (`None`
/// when larger than [`MAX_MIRRORED_BYTES`]) and its stamp.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub name: String,
    pub content: Option<Vec<u8>>,
    pub size: u64,
    pub stamp: String,
}

/// Everything the index keeps about one app, read from disk.
#[derive(Debug)]
pub struct AppSnapshot {
    pub meta: AppMeta,
    pub files: Vec<FileEntry>,
    pub git_stamp: String,
}

/// What `stat` alone says about one app — enough to tell whether its
/// indexed copy is still current.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct AppStamps {
    /// Logical file name -> stamp.
    pub files: BTreeMap<String, String>,
    pub git: String,
}

/// `inode:size:mtime_ns:ctime_ns`. The inode and ctime catch a file
/// replaced with its mtime preserved (`cp -p`, `rsync -t`, a tar
/// extraction): ctime cannot be set from userspace.
pub fn stamp_of(md: &fs::Metadata) -> String {
    format!(
        "{}:{}:{}:{}",
        md.ino(),
        md.len(),
        md.mtime() as i128 * 1_000_000_000 + md.mtime_nsec() as i128,
        md.ctime() as i128 * 1_000_000_000 + md.ctime_nsec() as i128,
    )
}

/// Whether a file of `.asc/` is part of the app's state. The operation
/// lock is not data, and a temporary file is an atomic write still in
/// flight — `meta.json.tmp`, or a hidden `.compose.override.yaml.tmp-<pid>`
/// candidate — whose final name shows up once the rename lands.
pub fn mirrored_name(name: &str) -> bool {
    name != crate::daemon::apps::oplock::LOCK_FILE
        && !name.ends_with(".tmp")
        && !(name.starts_with('.') && name.contains(".tmp"))
}

/// App ids under the apps root: directories whose name is a valid id. A
/// missing root is an empty store, not an error.
pub fn app_ids(root: &Path) -> io::Result<Vec<String>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        // Same rule as the filesystem listing: a directory (or a symlink
        // to one) whose name is a valid app id.
        if validate_id(&name).is_ok() && entry.path().is_dir() {
            ids.push(name);
        }
    }
    ids.sort();
    Ok(ids)
}

/// The files that make up an app's state, as `(logical name, path)`: every
/// regular file at the top of `.asc/`, plus the legacy `meta.json` at the
/// app root and `config/settings.json` while an app that could not be moved
/// into `.asc/` still keeps them there. Symlinks are skipped: the daemon
/// runs as root and must not copy whatever a link points at into its
/// database.
fn state_files(app_dir: &Path) -> io::Result<Vec<(String, PathBuf, fs::Metadata)>> {
    let mut files = Vec::new();
    let state = layout::state_dir(app_dir);
    match fs::read_dir(&state) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                if !mirrored_name(&name) {
                    continue;
                }
                // DirEntry::metadata does not follow symlinks.
                let md = match entry.metadata() {
                    Ok(md) => md,
                    // Removed between readdir and stat.
                    Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e),
                };
                if md.is_file() {
                    files.push((name, entry.path(), md));
                }
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let has = |files: &[(String, PathBuf, fs::Metadata)], name: &str| {
        files.iter().any(|(n, _, _)| n == name)
    };
    let legacy = [
        (AppMeta::FILE, layout::legacy_meta_path(app_dir)),
        (
            SettingValues::FILE,
            app_dir.join("config").join(SettingValues::FILE),
        ),
    ];
    for (name, path) in legacy {
        if has(&files, name) {
            continue;
        }
        match fs::symlink_metadata(&path) {
            Ok(md) if md.is_file() => files.push((name.to_string(), path, md)),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

/// Stamp of the repository's checked-out commit: `.git/HEAD` (rewritten by
/// a checkout), `.git/packed-refs` (a gc or fetch) and the `refs/heads`
/// directory (a branch ref replaced through its lock file). A changed stamp
/// is what makes a sync run `git rev-parse` again; an unchanged one reuses
/// the indexed commit.
pub fn git_stamp(app_dir: &Path) -> String {
    let git = app_dir.join("repository").join(".git");
    [
        git.join("HEAD"),
        git.join("packed-refs"),
        git.join("refs").join("heads"),
    ]
    .iter()
    .map(|path| match fs::symlink_metadata(path) {
        Ok(md) => stamp_of(&md),
        Err(_) => "-".to_string(),
    })
    .collect::<Vec<_>>()
    .join("|")
}

/// Stat-only view of one app; `None` when the directory holds no app.
pub fn stamps(app_dir: &Path) -> io::Result<Option<AppStamps>> {
    let files = state_files(app_dir)?;
    if !files.iter().any(|(name, _, _)| name == AppMeta::FILE) {
        return Ok(None);
    }
    Ok(Some(AppStamps {
        files: files
            .into_iter()
            .map(|(name, _, md)| (name, stamp_of(&md)))
            .collect(),
        git: git_stamp(app_dir),
    }))
}

/// Read one file for the index through a single open descriptor, so the
/// stamp and the content describe the same file even if it is replaced
/// meanwhile. `O_NOFOLLOW`: a symlink planted after the readdir is refused,
/// not followed.
fn read_entry(name: String, path: &Path, cap: Option<u64>) -> io::Result<FileEntry> {
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let md = file.metadata()?;
    if !md.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a file"));
    }
    let content = match cap {
        Some(cap) if md.len() > cap => None,
        _ => {
            let mut buf = Vec::with_capacity(md.len() as usize);
            file.read_to_end(&mut buf)?;
            Some(buf)
        }
    };
    Ok(FileEntry {
        name,
        content,
        size: md.len(),
        stamp: stamp_of(&md),
    })
}

/// Why an app directory could not be indexed.
#[derive(Debug)]
pub enum ReadError {
    /// No `meta.json` at all — not an app (or already removed).
    Missing,
    /// `meta.json` exists but cannot be used; the filesystem listing skips
    /// such an app with a warning, and so does the index.
    Broken(String),
    Io(io::Error),
}

impl From<io::Error> for ReadError {
    fn from(e: io::Error) -> Self {
        ReadError::Io(e)
    }
}

/// Read one app's full state. A legacy app is moved into `.asc/` first,
/// exactly as the filesystem listing always did; when that is refused the
/// legacy paths are read instead.
pub fn read_app(root: &Path, id: &str) -> Result<AppSnapshot, ReadError> {
    let app_dir = root.join(id);
    if layout::legacy_meta_path(&app_dir).exists() {
        crate::daemon::apps::store::migrate(&app_dir);
    }
    let paths = match state_files(&app_dir) {
        Ok(paths) => paths,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(ReadError::Missing),
        Err(e) => return Err(e.into()),
    };
    let mut files = Vec::with_capacity(paths.len());
    for (name, path, _) in paths {
        let cap = (name != AppMeta::FILE).then_some(MAX_MIRRORED_BYTES);
        match read_entry(name, &path, cap) {
            Ok(entry) => files.push(entry),
            // Removed (or swapped for a symlink) since the readdir.
            Err(e)
                if e.kind() == io::ErrorKind::NotFound
                    || e.raw_os_error() == Some(libc::ELOOP)
                    || e.kind() == io::ErrorKind::InvalidInput => {}
            Err(e) => return Err(e.into()),
        }
    }
    let Some(raw) = files
        .iter()
        .find(|f| f.name == AppMeta::FILE)
        .and_then(|f| f.content.as_deref())
    else {
        return Err(ReadError::Missing);
    };
    let meta: AppMeta = serde_json::from_slice(raw)
        .map_err(|e| ReadError::Broken(format!("invalid meta.json: {e}")))?;
    if meta.id != id {
        return Err(ReadError::Broken(format!(
            "meta.json id '{}' does not match directory name",
            meta.id
        )));
    }
    Ok(AppSnapshot {
        meta,
        files,
        git_stamp: git_stamp(&app_dir),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_meta(dir: &Path, id: &str) {
        fs::create_dir_all(dir.join(".asc")).unwrap();
        let meta = serde_json::json!({
            "id": id,
            "name": id,
            "owner": { "uid": 1000, "name": "user" },
            "runtime": { "type": "process", "command": "true" },
        });
        fs::write(dir.join(".asc/meta.json"), meta.to_string()).unwrap();
    }

    #[test]
    fn mirrors_top_level_files_but_not_locks_tmp_or_subdirs() {
        let root = tempfile::tempdir().unwrap();
        let app = root.path().join("web");
        write_meta(&app, "web");
        fs::write(app.join(".asc/settings.json"), b"{\"A\":1}").unwrap();
        fs::write(app.join(".asc/compose.override.yaml"), b"services: {}").unwrap();
        fs::write(app.join(".asc/op.lock"), b"").unwrap();
        fs::write(app.join(".asc/settings.json.tmp"), b"half").unwrap();
        fs::create_dir_all(app.join(".asc/backups/old")).unwrap();
        fs::write(app.join(".asc/backups/x.patch"), b"diff").unwrap();

        let snap = read_app(root.path(), "web").unwrap();
        let names: Vec<_> = snap.files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            ["compose.override.yaml", "meta.json", "settings.json"]
        );
        let settings = snap
            .files
            .iter()
            .find(|f| f.name == "settings.json")
            .unwrap();
        assert_eq!(settings.content.as_deref(), Some(&b"{\"A\":1}"[..]));
    }

    #[test]
    fn large_files_are_indexed_without_content() {
        let root = tempfile::tempdir().unwrap();
        let app = root.path().join("big");
        write_meta(&app, "big");
        fs::write(
            app.join(".asc/huge.bin"),
            vec![0u8; MAX_MIRRORED_BYTES as usize + 1],
        )
        .unwrap();
        let snap = read_app(root.path(), "big").unwrap();
        let huge = snap.files.iter().find(|f| f.name == "huge.bin").unwrap();
        assert!(huge.content.is_none());
        assert_eq!(huge.size, MAX_MIRRORED_BYTES + 1);
    }

    #[test]
    fn symlinks_in_state_dir_are_not_followed() {
        let root = tempfile::tempdir().unwrap();
        let app = root.path().join("web");
        write_meta(&app, "web");
        let secret = root.path().join("secret");
        fs::write(&secret, b"top secret").unwrap();
        std::os::unix::fs::symlink(&secret, app.join(".asc/settings.json")).unwrap();
        let snap = read_app(root.path(), "web").unwrap();
        assert!(snap.files.iter().all(|f| f.name != "settings.json"));
    }

    #[test]
    fn mismatched_or_broken_meta_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        write_meta(&root.path().join("other"), "web");
        assert!(matches!(
            read_app(root.path(), "other"),
            Err(ReadError::Broken(_))
        ));
        let bad = root.path().join("bad");
        fs::create_dir_all(bad.join(".asc")).unwrap();
        fs::write(bad.join(".asc/meta.json"), b"{ nope").unwrap();
        assert!(matches!(
            read_app(root.path(), "bad"),
            Err(ReadError::Broken(_))
        ));
        fs::create_dir_all(root.path().join("empty")).unwrap();
        assert!(matches!(
            read_app(root.path(), "empty"),
            Err(ReadError::Missing)
        ));
    }

    #[test]
    fn stamps_change_when_a_file_is_replaced_with_the_same_mtime() {
        let root = tempfile::tempdir().unwrap();
        let app = root.path().join("web");
        write_meta(&app, "web");
        let path = app.join(".asc/settings.json");
        fs::write(&path, b"{\"A\":1}").unwrap();
        let before = stamps(&app).unwrap().unwrap();
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();

        // Same size, same mtime, different file (what `cp -p` produces).
        let tmp = app.join(".asc/replacement");
        fs::write(&tmp, b"{\"A\":2}").unwrap();
        fs::File::options()
            .write(true)
            .open(&tmp)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        fs::rename(&tmp, &path).unwrap();

        let after = stamps(&app).unwrap().unwrap();
        assert_ne!(before.files["settings.json"], after.files["settings.json"]);
    }

    #[test]
    fn app_ids_lists_valid_directories_only() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("web")).unwrap();
        fs::create_dir_all(root.path().join("Not Valid")).unwrap();
        fs::write(root.path().join("file"), b"").unwrap();
        assert_eq!(app_ids(root.path()).unwrap(), ["web"]);
        assert!(app_ids(&root.path().join("missing")).unwrap().is_empty());
    }
}
