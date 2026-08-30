//! Node filesystem access (DMN-070): list, stat, create, move, copy, delete,
//! archive and stream any path from `/`. See docs/files.md.
//!
//! The daemon runs as root, so this service sees the whole filesystem — the
//! platform performs its own per-user authorization (`files.read`/
//! `files.edit`) before a request ever reaches here. Because of that, every
//! entry point requires a root caller [`UserContext`] (see [`require_root`]):
//! the unix socket is otherwise world-connectable and authorizes purely by
//! peer uid, a rule this service must not inherit.

pub mod path;
mod walk;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::daemon::apps::UserContext;

pub use path::SafePath;

/// Chunk size for `ReadFile`/`WriteFile` streaming: comfortably under
/// tonic's 4 MiB default decode limit, large enough to amortize per-syscall
/// and per-HTTP/2-frame overhead.
pub const CHUNK_BYTES: usize = 256 * 1024;

/// Directory listing cap; past this, [`Listing::truncated`] is set.
pub const MAX_LISTING_ENTRIES: usize = 10_000;

/// Refused for recursive delete/copy/archive and as a move source or
/// destination: sizes under these are fiction and a walk here can hang
/// forever. Listing is fine.
const PSEUDO_ROOTS: &[&str] = &["/proc", "/sys", "/dev", "/run"];

/// Refused as the exact target of a destructive operation. A guard rail
/// against a mis-click, not a security boundary — root on the machine can
/// still do the same thing by hand.
const PROTECTED_PATHS: &[&str] = &["/", "/boot", "/etc", "/usr", "/var", "/asc"];

fn is_protected(path: &Path) -> bool {
    PROTECTED_PATHS.iter().any(|p| path == Path::new(p))
}

fn in_pseudo_root(path: &Path) -> bool {
    PSEUDO_ROOTS
        .iter()
        .any(|root| path == Path::new(root) || path.starts_with(root))
}

/// Typed file-operation error, downcastable by the gRPC/REST transports so
/// "already exists" reaches the caller as such instead of collapsing into a
/// generic internal error.
#[derive(Debug)]
pub enum FileError {
    NotFound(PathBuf),
    Exists(PathBuf),
    PermissionDenied(PathBuf),
    InvalidPath(String),
    NotADirectory(PathBuf),
    IsADirectory(PathBuf),
    DirectoryNotEmpty(PathBuf),
    DestinationInsideSource {
        source: PathBuf,
        destination: PathBuf,
    },
    /// A protected path, a pseudo-root, or (via [`require_root`]) a caller
    /// without a root context.
    Protected(PathBuf),
    /// `chown`'s target user name has no `/etc/passwd` entry.
    UnknownUser(String),
    /// `chown`'s target group name has no `/etc/group` entry.
    UnknownGroup(String),
    Io(PathBuf, std::io::Error),
}

impl FileError {
    /// Classify an I/O error against the path it happened on. Catches the
    /// common cases (missing, exists, denied, ENOTEMPTY) so most call sites
    /// never need their own mapping.
    fn io(path: &Path, err: std::io::Error) -> Self {
        match err.kind() {
            std::io::ErrorKind::NotFound => return FileError::NotFound(path.to_path_buf()),
            std::io::ErrorKind::PermissionDenied => {
                return FileError::PermissionDenied(path.to_path_buf());
            }
            std::io::ErrorKind::AlreadyExists => return FileError::Exists(path.to_path_buf()),
            _ => {}
        }
        if err.raw_os_error() == Some(libc::ENOTEMPTY) {
            return FileError::DirectoryNotEmpty(path.to_path_buf());
        }
        FileError::Io(path.to_path_buf(), err)
    }
}

impl std::fmt::Display for FileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FileError::NotFound(p) => write!(f, "not found: {}", p.display()),
            FileError::Exists(p) => write!(f, "already exists: {}", p.display()),
            FileError::PermissionDenied(p) => write!(f, "permission denied: {}", p.display()),
            FileError::InvalidPath(msg) => write!(f, "invalid path: {msg}"),
            FileError::NotADirectory(p) => write!(f, "not a directory: {}", p.display()),
            FileError::IsADirectory(p) => write!(f, "is a directory: {}", p.display()),
            FileError::DirectoryNotEmpty(p) => write!(f, "directory not empty: {}", p.display()),
            FileError::DestinationInsideSource {
                source,
                destination,
            } => write!(
                f,
                "destination {} is inside source {}",
                destination.display(),
                source.display()
            ),
            FileError::Protected(p) => write!(f, "path is protected: {}", p.display()),
            FileError::UnknownUser(name) => write!(f, "unknown user: {name}"),
            FileError::UnknownGroup(name) => write!(f, "unknown group: {name}"),
            FileError::Io(p, err) => write!(f, "{}: {err}", p.display()),
        }
    }
}

impl std::error::Error for FileError {}

pub type Result<T> = std::result::Result<T, FileError>;

/// Refuse anything other than a full-visibility caller. TCP (the platform)
/// always presents [`crate::daemon::api::api_context`], so this only ever
/// bites the unix socket, which is otherwise world-connectable.
pub fn require_root(ctx: &UserContext) -> Result<()> {
    if ctx.is_root {
        Ok(())
    } else {
        Err(FileError::Protected(PathBuf::from("/")))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    Directory,
    Symlink,
    Other,
}

/// One directory entry as lstat sees it: a symlink is always [`FileKind::Symlink`],
/// never the kind of what it points at.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub name: String,
    pub kind: FileKind,
    pub size: u64,
    pub modified_at: i64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub owner: String,
    pub group: String,
    pub is_symlink: bool,
    pub symlink_target: Option<String>,
    pub target_kind: Option<FileKind>,
}

pub struct Listing {
    pub path: String,
    pub entries: Vec<FileEntry>,
    pub truncated: bool,
    pub total_entries: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    TarGz,
    /// Reserved: rejected with [`FileError::InvalidPath`] until a zip
    /// encoder is worth the added dependency (see docs/files.md).
    Zip,
}

// ── owner/group name resolution, memoized per listing ──

struct NameCache {
    users: HashMap<u32, String>,
    groups: HashMap<u32, String>,
}

impl NameCache {
    fn new() -> Self {
        Self {
            users: HashMap::new(),
            groups: HashMap::new(),
        }
    }

    fn user(&mut self, uid: u32) -> String {
        self.users
            .entry(uid)
            .or_insert_with(|| user_name(uid))
            .clone()
    }

    fn group(&mut self, gid: u32) -> String {
        self.groups
            .entry(gid)
            .or_insert_with(|| group_name(gid))
            .clone()
    }
}

fn user_name(uid: u32) -> String {
    passwd_name(uid).unwrap_or_else(|| uid.to_string())
}

fn group_name(gid: u32) -> String {
    group_name_lookup(gid).unwrap_or_else(|| gid.to_string())
}

fn passwd_name(uid: u32) -> Option<String> {
    let mut buf = vec![0i8; 4096];
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: all pointers reference live buffers of the stated sizes.
    let rc = unsafe {
        libc::getpwuid_r(
            uid,
            &mut pwd,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() {
        return None;
    }
    // SAFETY: on success pw_name points at a NUL-terminated string in buf.
    Some(
        unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) }
            .to_string_lossy()
            .into_owned(),
    )
}

fn group_name_lookup(gid: u32) -> Option<String> {
    let mut buf = vec![0i8; 4096];
    let mut grp: libc::group = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::group = std::ptr::null_mut();
    // SAFETY: all pointers reference live buffers of the stated sizes.
    let rc = unsafe {
        libc::getgrgid_r(
            gid,
            &mut grp,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() {
        return None;
    }
    // SAFETY: on success gr_name points at a NUL-terminated string in buf.
    Some(
        unsafe { std::ffi::CStr::from_ptr(grp.gr_name) }
            .to_string_lossy()
            .into_owned(),
    )
}

fn kind_of(meta: &std::fs::Metadata) -> FileKind {
    if meta.is_dir() {
        FileKind::Directory
    } else if meta.is_file() {
        FileKind::File
    } else {
        FileKind::Other
    }
}

fn unix_mode(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    meta.mode() & 0o7777
}

fn unix_ids(meta: &std::fs::Metadata) -> (u32, u32) {
    use std::os::unix::fs::MetadataExt;
    (meta.uid(), meta.gid())
}

fn unix_modified(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn describe(path: &Path) -> Result<FileEntry> {
    let mut cache = NameCache::new();
    describe_with_cache(path, &mut cache)
}

fn describe_with_cache(path: &Path, cache: &mut NameCache) -> Result<FileEntry> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| FileError::io(path, e))?;
    let is_symlink = meta.file_type().is_symlink();
    let (kind, symlink_target, target_kind) = if is_symlink {
        let target = std::fs::read_link(path)
            .ok()
            .map(|p| p.to_string_lossy().into_owned());
        // Best-effort: a broken link (or one denied by a parent directory's
        // permissions) simply reports no target kind, not an error.
        let target_kind = std::fs::metadata(path).ok().map(|m| kind_of(&m));
        (FileKind::Symlink, target, target_kind)
    } else {
        (kind_of(&meta), None, None)
    };
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/".to_string());
    let (uid, gid) = unix_ids(&meta);
    Ok(FileEntry {
        name,
        kind,
        // A symlink's own size (the length of its target string) is not
        // meaningful to a file-manager UI; report 0, matching how a symlink
        // never contributes bytes to a recursive size walk either.
        size: if is_symlink { 0 } else { meta.len() },
        modified_at: unix_modified(&meta),
        mode: unix_mode(&meta),
        uid,
        gid,
        owner: cache.user(uid),
        group: cache.group(gid),
        is_symlink,
        symlink_target,
        target_kind,
    })
}

/// List a directory's entries. Directories first, then by name
/// case-insensitively; capped at [`MAX_LISTING_ENTRIES`]. An entry that
/// vanishes or cannot be `lstat`ed between `readdir` and inspection is
/// skipped rather than failing the whole listing.
pub fn list_directory(raw_path: &str, include_hidden: bool) -> Result<Listing> {
    let safe = SafePath::parse(raw_path)?;
    let meta =
        std::fs::symlink_metadata(safe.as_path()).map_err(|e| FileError::io(safe.as_path(), e))?;
    if !meta.is_dir() {
        return Err(FileError::NotADirectory(safe.as_path().to_path_buf()));
    }
    let mut cache = NameCache::new();
    let mut entries = Vec::new();
    let dir = std::fs::read_dir(safe.as_path()).map_err(|e| FileError::io(safe.as_path(), e))?;
    for entry in dir {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if !include_hidden && name.starts_with('.') {
            continue;
        }
        if let Ok(descr) = describe_with_cache(&entry.path(), &mut cache) {
            entries.push(descr);
        }
    }
    let total_entries = entries.len() as u64;
    entries.sort_by(|a, b| {
        let a_dir = a.kind == FileKind::Directory;
        let b_dir = b.kind == FileKind::Directory;
        b_dir
            .cmp(&a_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    let truncated = entries.len() > MAX_LISTING_ENTRIES;
    entries.truncate(MAX_LISTING_ENTRIES);
    Ok(Listing {
        path: safe.as_path().display().to_string(),
        entries,
        truncated,
        total_entries,
    })
}

/// Metadata for one path, plus its parent (for a UI's "go up" button).
pub fn stat(raw_path: &str) -> Result<(FileEntry, String)> {
    let safe = SafePath::parse(raw_path)?;
    let entry = describe(safe.as_path())?;
    let parent = safe
        .parent()
        .map(|p| p.as_path().display().to_string())
        .unwrap_or_else(|| "/".to_string());
    Ok((entry, parent))
}

pub fn create_directory(raw_path: &str, parents: bool) -> Result<FileEntry> {
    let safe = SafePath::parse(raw_path)?;
    if is_protected(safe.as_path()) {
        return Err(FileError::Protected(safe.as_path().to_path_buf()));
    }
    let result = if parents {
        std::fs::create_dir_all(safe.as_path())
    } else {
        std::fs::create_dir(safe.as_path())
    };
    result.map_err(|e| FileError::io(safe.as_path(), e))?;
    describe(safe.as_path())
}

/// Rename/move `source` to `destination`. A rename is just a move whose
/// destination shares the source's parent — same function, same rule.
pub fn move_path(raw_source: &str, raw_destination: &str, overwrite: bool) -> Result<FileEntry> {
    let src = SafePath::parse(raw_source)?;
    let dst = SafePath::parse(raw_destination)?;
    if is_protected(src.as_path())
        || is_protected(dst.as_path())
        || in_pseudo_root(src.as_path())
        || in_pseudo_root(dst.as_path())
    {
        return Err(FileError::Protected(dst.as_path().to_path_buf()));
    }
    if dst.as_path().starts_with(src.as_path()) {
        return Err(FileError::DestinationInsideSource {
            source: src.as_path().to_path_buf(),
            destination: dst.as_path().to_path_buf(),
        });
    }
    if !overwrite && dst.as_path().exists() {
        return Err(FileError::Exists(dst.as_path().to_path_buf()));
    }
    std::fs::rename(src.as_path(), dst.as_path()).map_err(|e| FileError::io(dst.as_path(), e))?;
    describe(dst.as_path())
}

pub fn copy_path(
    raw_source: &str,
    raw_destination: &str,
    overwrite: bool,
) -> Result<(FileEntry, u64, u32)> {
    let src = SafePath::parse(raw_source)?;
    let dst = SafePath::parse(raw_destination)?;
    if is_protected(dst.as_path()) || in_pseudo_root(src.as_path()) {
        return Err(FileError::Protected(dst.as_path().to_path_buf()));
    }
    if dst.as_path().starts_with(src.as_path()) {
        return Err(FileError::DestinationInsideSource {
            source: src.as_path().to_path_buf(),
            destination: dst.as_path().to_path_buf(),
        });
    }
    if !overwrite && dst.as_path().exists() {
        return Err(FileError::Exists(dst.as_path().to_path_buf()));
    }
    let (files, bytes) = walk::copy_recursive(src.as_path(), dst.as_path())?;
    let entry = describe(dst.as_path())?;
    Ok((entry, bytes, files))
}

/// Delete every path in `paths`, best-effort: one refusal does not abort the
/// rest. Returns `(deleted count, [(path, error message)])`.
pub fn delete_paths(paths: &[String], recursive: bool) -> (u32, Vec<(String, String)>) {
    let mut deleted = 0u32;
    let mut failures = Vec::new();
    for raw in paths {
        match delete_one(raw, recursive) {
            Ok(()) => deleted += 1,
            Err(err) => failures.push((raw.clone(), err.to_string())),
        }
    }
    (deleted, failures)
}

fn delete_one(raw_path: &str, recursive: bool) -> Result<()> {
    let safe = SafePath::parse(raw_path)?;
    let path = safe.as_path();
    if is_protected(path) {
        return Err(FileError::Protected(path.to_path_buf()));
    }
    let meta = std::fs::symlink_metadata(path).map_err(|e| FileError::io(path, e))?;
    if meta.file_type().is_symlink() {
        // Unlinks the link itself; the target is never touched.
        return std::fs::remove_file(path).map_err(|e| FileError::io(path, e));
    }
    if meta.is_dir() {
        if recursive && in_pseudo_root(path) {
            return Err(FileError::Protected(path.to_path_buf()));
        }
        if recursive {
            // std::fs::remove_dir_all never follows a symlink it encounters:
            // it classifies entries by lstat, unlinking a symlink rather
            // than recursing through it — the same policy this module
            // enforces everywhere else, for free.
            std::fs::remove_dir_all(path).map_err(|e| FileError::io(path, e))
        } else {
            std::fs::remove_dir(path).map_err(|e| FileError::io(path, e))
        }
    } else {
        std::fs::remove_file(path).map_err(|e| FileError::io(path, e))
    }
}

pub fn create_archive(
    raw_directory: &str,
    names: &[String],
    raw_archive_path: &str,
    format: ArchiveFormat,
) -> Result<(FileEntry, u64, u32)> {
    if format != ArchiveFormat::TarGz {
        return Err(FileError::InvalidPath(
            "only the tar.gz archive format is supported".into(),
        ));
    }
    let dir = SafePath::parse(raw_directory)?;
    if in_pseudo_root(dir.as_path()) {
        return Err(FileError::Protected(dir.as_path().to_path_buf()));
    }
    let archive = SafePath::parse(raw_archive_path)?;
    if is_protected(archive.as_path()) {
        return Err(FileError::Protected(archive.as_path().to_path_buf()));
    }

    let mut file_count = 0u32;
    {
        let file = std::fs::File::create(archive.as_path())
            .map_err(|e| FileError::io(archive.as_path(), e))?;
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            file,
            flate2::Compression::default(),
        ));
        for name in names {
            let child = dir.child(name)?;
            file_count += walk::append_named(&mut builder, child.as_path(), name)?;
        }
        builder
            .into_inner()
            .map_err(|e| FileError::io(archive.as_path(), e))?
            .finish()
            .map_err(|e| FileError::io(archive.as_path(), e))?;
    }
    let bytes = std::fs::metadata(archive.as_path())
        .map(|m| m.len())
        .unwrap_or(0);
    let entry = describe(archive.as_path())?;
    Ok((entry, bytes, file_count))
}

// ── ownership and permissions ──

/// One local Linux user, from `/etc/passwd` — for a UI that lets an operator
/// reassign ownership by name instead of a raw uid.
#[derive(Debug, Clone)]
pub struct SystemUser {
    pub name: String,
    pub uid: u32,
    pub home: String,
}

/// One local Linux group, from `/etc/group`.
#[derive(Debug, Clone)]
pub struct SystemGroup {
    pub name: String,
    pub gid: u32,
}

/// Every local user and group, ascending by id.
pub fn list_system_identities() -> Result<(Vec<SystemUser>, Vec<SystemGroup>)> {
    Ok((parse_passwd()?, parse_group()?))
}

/// Parses `/etc/passwd` directly rather than `getpwent`: that iterates
/// global libc state with no thread-safety story of its own, and every
/// caller here already runs on its own `spawn_blocking` worker thread.
fn parse_passwd() -> Result<Vec<SystemUser>> {
    let path = Path::new("/etc/passwd");
    let raw = std::fs::read_to_string(path).map_err(|e| FileError::io(path, e))?;
    let mut users: Vec<SystemUser> = raw
        .lines()
        .filter_map(|line| {
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            // name:password:uid:gid:gecos:home:shell
            let mut fields = line.split(':');
            let name = fields.next()?.to_string();
            fields.next()?;
            let uid: u32 = fields.next()?.parse().ok()?;
            fields.next()?;
            fields.next()?;
            let home = fields.next().unwrap_or_default().to_string();
            Some(SystemUser { name, uid, home })
        })
        .collect();
    users.sort_by_key(|u| u.uid);
    Ok(users)
}

/// Mirrors [`parse_passwd`] for `/etc/group`: `name:password:gid:members`.
fn parse_group() -> Result<Vec<SystemGroup>> {
    let path = Path::new("/etc/group");
    let raw = std::fs::read_to_string(path).map_err(|e| FileError::io(path, e))?;
    let mut groups: Vec<SystemGroup> = raw
        .lines()
        .filter_map(|line| {
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let mut fields = line.split(':');
            let name = fields.next()?.to_string();
            fields.next()?;
            let gid: u32 = fields.next()?.parse().ok()?;
            Some(SystemGroup { name, gid })
        })
        .collect();
    groups.sort_by_key(|g| g.gid);
    Ok(groups)
}

fn uid_for_name(name: &str) -> Result<u32> {
    parse_passwd()?
        .into_iter()
        .find(|u| u.name == name)
        .map(|u| u.uid)
        .ok_or_else(|| FileError::UnknownUser(name.to_string()))
}

fn gid_for_name(name: &str) -> Result<u32> {
    parse_group()?
        .into_iter()
        .find(|g| g.name == name)
        .map(|g| g.gid)
        .ok_or_else(|| FileError::UnknownGroup(name.to_string()))
}

/// Change mode and/or owner/group; a `None` field leaves that attribute
/// untouched — `chmod`/`chown` semantics, not "reset to a default". Owner
/// and group are names resolved against `/etc/passwd`/`/etc/group` (a UI
/// dropdown from [`list_system_identities`], not a raw uid/gid from a
/// script). Chowning a symlink follows it, matching plain `chown` on the
/// shell — `chown -h` is not exposed; a file manager operates on what a
/// path resolves to.
pub fn set_attributes(
    raw_path: &str,
    mode: Option<u32>,
    owner: Option<&str>,
    group: Option<&str>,
) -> Result<FileEntry> {
    let safe = SafePath::parse(raw_path)?;
    let path = safe.as_path();
    if is_protected(path) {
        return Err(FileError::Protected(path.to_path_buf()));
    }
    if owner.is_some() || group.is_some() {
        // (uid_t)-1 / (gid_t)-1 is POSIX's own sentinel for "leave this half
        // of ownership unchanged" — u32::MAX is that value reinterpreted.
        let uid = owner.map(uid_for_name).transpose()?.unwrap_or(u32::MAX);
        let gid = group.map(gid_for_name).transpose()?.unwrap_or(u32::MAX);
        use std::os::unix::ffi::OsStrExt;
        let cpath = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| FileError::InvalidPath("path contains a NUL byte".into()))?;
        // SAFETY: cpath is a valid, NUL-terminated C string for the
        // duration of the call; chown cannot corrupt Rust-side state.
        let rc = unsafe { libc::chown(cpath.as_ptr(), uid, gid) };
        if rc != 0 {
            return Err(FileError::io(path, std::io::Error::last_os_error()));
        }
    }
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o7777))
            .map_err(|e| FileError::io(path, e))?;
    }
    describe(path)
}

// ── streaming: read/write handles used by the service layer's
//    spawn_blocking + mpsc bridge (byte transfer does not fit the
//    single-result `ApiState::blocking` helper) ──

/// An open file ready to be streamed out, chunk by chunk, on a blocking
/// worker thread.
pub struct ReadHandle {
    pub size: u64,
    file: std::fs::File,
}

impl ReadHandle {
    pub fn open(raw_path: &str, offset: u64) -> Result<Self> {
        let safe = SafePath::parse(raw_path)?;
        let meta = std::fs::symlink_metadata(safe.as_path())
            .map_err(|e| FileError::io(safe.as_path(), e))?;
        if meta.is_dir() {
            return Err(FileError::IsADirectory(safe.as_path().to_path_buf()));
        }
        let mut file =
            std::fs::File::open(safe.as_path()).map_err(|e| FileError::io(safe.as_path(), e))?;
        if offset > 0 {
            use std::io::Seek;
            file.seek(std::io::SeekFrom::Start(offset))
                .map_err(|e| FileError::io(safe.as_path(), e))?;
        }
        Ok(Self {
            size: meta.len(),
            file,
        })
    }

    /// Read up to `buf.len()` bytes; `Ok(0)` means end of file.
    pub fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::io::Read;
        self.file.read(buf)
    }
}

/// Where an upload lands: `directory`/`name`, staged first at a temporary
/// path beside it. `overwrite` gates a pre-existing `directory`/`name`.
pub struct WriteHeader {
    pub directory: String,
    pub name: String,
    pub overwrite: bool,
    pub mode: Option<u32>,
}

/// An in-progress upload. Bytes land in a temporary file next to the target;
/// [`WriteHandle::commit`] fsyncs and atomically renames it into place. If
/// the handle is dropped without being committed — an aborted upload, a
/// failed write — the temporary file is removed: an interrupted upload must
/// never leave a truncated file where a good one used to be.
pub struct WriteHandle {
    temp_path: PathBuf,
    final_path: PathBuf,
    mode: Option<u32>,
    file: Option<std::fs::File>,
}

impl WriteHandle {
    pub fn open(header: &WriteHeader) -> Result<Self> {
        let dir = SafePath::parse(&header.directory)?;
        let dir_meta = std::fs::symlink_metadata(dir.as_path())
            .map_err(|e| FileError::io(dir.as_path(), e))?;
        if !dir_meta.is_dir() {
            return Err(FileError::NotADirectory(dir.as_path().to_path_buf()));
        }
        let target = dir.child(&header.name)?;
        if is_protected(target.as_path()) {
            return Err(FileError::Protected(target.as_path().to_path_buf()));
        }
        // Checked before the first byte is accepted: a collision must not
        // surface only after the upload has already streamed to disk.
        if !header.overwrite && target.as_path().exists() {
            return Err(FileError::Exists(target.as_path().to_path_buf()));
        }
        let temp_name = format!(
            ".asc-upload-{}.part",
            crate::daemon::api::console::random_hex(8)
        );
        let temp_path = dir.as_path().join(temp_name);
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp_path)
            .map_err(|e| FileError::io(&temp_path, e))?;
        Ok(Self {
            temp_path,
            final_path: target.as_path().to_path_buf(),
            mode: header.mode,
            file: Some(file),
        })
    }

    pub fn write_all(&mut self, data: &[u8]) -> Result<()> {
        use std::io::Write;
        let file = self.file.as_mut().expect("write after commit");
        file.write_all(data)
            .map_err(|e| FileError::io(&self.temp_path, e))
    }

    /// Fsync, apply the requested mode (default 0644), and rename into
    /// place.
    pub fn commit(mut self) -> Result<FileEntry> {
        let file = self.file.take().expect("commit called twice");
        file.sync_all()
            .map_err(|e| FileError::io(&self.temp_path, e))?;
        drop(file);
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = self.mode.unwrap_or(0o644) & 0o7777;
            std::fs::set_permissions(&self.temp_path, std::fs::Permissions::from_mode(mode))
                .map_err(|e| FileError::io(&self.temp_path, e))?;
        }
        std::fs::rename(&self.temp_path, &self.final_path)
            .map_err(|e| FileError::io(&self.final_path, e))?;
        describe(&self.final_path)
    }
}

impl Drop for WriteHandle {
    fn drop(&mut self) {
        // `file` is only `None` after a successful commit (which already
        // renamed the temp path away); still holding it means the upload
        // never finished, so its partial bytes must not be mistaken for a
        // complete file.
        if self.file.is_some() {
            let _ = std::fs::remove_file(&self.temp_path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> UserContext {
        UserContext {
            uid: 0,
            name: "root".into(),
            is_root: true,
        }
    }

    fn user() -> UserContext {
        UserContext {
            uid: 1000,
            name: "user".into(),
            is_root: false,
        }
    }

    #[test]
    fn require_root_gates_non_root_callers() {
        assert!(require_root(&root()).is_ok());
        assert!(require_root(&user()).is_err());
    }

    #[test]
    fn list_directory_reports_symlinks_and_respects_hidden() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("visible.txt"), b"x").unwrap();
        std::fs::write(dir.path().join(".hidden"), b"x").unwrap();
        std::os::unix::fs::symlink("/nonexistent-target", dir.path().join("broken-link")).unwrap();
        std::os::unix::fs::symlink("/etc", dir.path().join("etc-link")).unwrap();

        let listing = list_directory(&dir.path().display().to_string(), false).unwrap();
        let names: Vec<_> = listing.entries.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&"visible.txt".to_string()));
        assert!(!names.contains(&".hidden".to_string()));

        let broken = listing
            .entries
            .iter()
            .find(|e| e.name == "broken-link")
            .unwrap();
        assert_eq!(broken.kind, FileKind::Symlink);
        assert!(broken.is_symlink);
        assert_eq!(
            broken.symlink_target.as_deref(),
            Some("/nonexistent-target")
        );
        assert!(broken.target_kind.is_none());

        let etc_link = listing
            .entries
            .iter()
            .find(|e| e.name == "etc-link")
            .unwrap();
        assert_eq!(
            etc_link.kind,
            FileKind::Symlink,
            "a symlink is never reported as its target's kind"
        );
        assert_eq!(etc_link.target_kind, Some(FileKind::Directory));

        let listing_hidden = list_directory(&dir.path().display().to_string(), true).unwrap();
        let names: Vec<_> = listing_hidden
            .entries
            .iter()
            .map(|e| e.name.clone())
            .collect();
        assert!(names.contains(&".hidden".to_string()));
    }

    #[test]
    fn list_directory_truncates_past_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        // A small cap would be nicer to test against, but MAX_LISTING_ENTRIES
        // is a `pub const`; exercise the truncation flag logic directly
        // instead of writing 10,001 files.
        for i in 0..5 {
            std::fs::write(dir.path().join(format!("f{i}")), b"").unwrap();
        }
        let listing = list_directory(&dir.path().display().to_string(), false).unwrap();
        assert_eq!(listing.total_entries, 5);
        assert!(!listing.truncated);
    }

    #[test]
    fn delete_paths_refuses_nonempty_dir_without_recursive() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("a.txt"), b"x").unwrap();

        let (deleted, failures) = delete_paths(&[sub.display().to_string()], false);
        assert_eq!(deleted, 0);
        assert_eq!(failures.len(), 1);
        assert!(sub.exists());

        let (deleted, failures) = delete_paths(&[sub.display().to_string()], true);
        assert_eq!(deleted, 1);
        assert!(failures.is_empty());
        assert!(!sub.exists());
    }

    #[test]
    fn delete_paths_never_follows_a_symlink_out() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("keepme.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();

        let (deleted, failures) = delete_paths(&[dir.path().display().to_string()], true);
        assert_eq!(deleted, 1);
        assert!(failures.is_empty());
        assert!(
            outside.path().join("keepme.txt").exists(),
            "deleting a directory containing a symlink must not delete through it"
        );
    }

    #[test]
    fn delete_paths_is_best_effort_across_multiple_paths() {
        let dir = tempfile::tempdir().unwrap();
        let ok = dir.path().join("ok.txt");
        std::fs::write(&ok, b"x").unwrap();
        let missing = dir.path().join("missing.txt");

        let (deleted, failures) = delete_paths(
            &[ok.display().to_string(), missing.display().to_string()],
            false,
        );
        assert_eq!(deleted, 1);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, missing.display().to_string());
    }

    #[test]
    fn move_and_copy_refuse_to_clobber_without_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("dst.txt");
        std::fs::write(&src, b"source").unwrap();
        std::fs::write(&dst, b"existing").unwrap();

        assert!(
            move_path(
                &src.display().to_string(),
                &dst.display().to_string(),
                false
            )
            .is_err()
        );
        assert!(
            copy_path(
                &src.display().to_string(),
                &dst.display().to_string(),
                false
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&dst).unwrap(), b"existing");

        move_path(&src.display().to_string(), &dst.display().to_string(), true).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"source");
    }

    #[test]
    fn copy_refuses_destination_inside_source() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("tree");
        std::fs::create_dir(&src).unwrap();
        let dst = src.join("nested");

        let err =
            copy_path(&src.display().to_string(), &dst.display().to_string(), true).unwrap_err();
        assert!(matches!(err, FileError::DestinationInsideSource { .. }));
    }

    #[test]
    fn create_archive_contains_exactly_the_requested_names_and_no_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"1").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"22").unwrap();
        std::os::unix::fs::symlink("/etc", dir.path().join("link")).unwrap();
        let archive_path = dir.path().join("out.tar.gz");

        let (entry, bytes, files) = create_archive(
            &dir.path().display().to_string(),
            &["a.txt".to_string(), "link".to_string()],
            &archive_path.display().to_string(),
            ArchiveFormat::TarGz,
        )
        .unwrap();
        assert_eq!(entry.name, "out.tar.gz");
        assert!(bytes > 0);
        assert_eq!(files, 1, "the symlink contributes no archive member");

        let file = std::fs::File::open(&archive_path).unwrap();
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
        let names: Vec<String> = archive
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.txt".to_string()]);
    }

    #[test]
    fn write_handle_leaves_no_part_file_when_dropped_uncommitted() {
        let dir = tempfile::tempdir().unwrap();
        let header = WriteHeader {
            directory: dir.path().display().to_string(),
            name: "upload.bin".into(),
            overwrite: false,
            mode: None,
        };
        {
            let mut handle = WriteHandle::open(&header).unwrap();
            handle.write_all(b"partial").unwrap();
            // Dropped without commit — the aborted upload's temp file must
            // not survive it.
        }
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(
            leftovers.is_empty(),
            "expected no leftover files, found {leftovers:?}"
        );
    }

    #[test]
    fn write_handle_commits_atomically_with_default_mode() {
        let dir = tempfile::tempdir().unwrap();
        let header = WriteHeader {
            directory: dir.path().display().to_string(),
            name: "upload.bin".into(),
            overwrite: false,
            mode: None,
        };
        let mut handle = WriteHandle::open(&header).unwrap();
        handle.write_all(b"hello ").unwrap();
        handle.write_all(b"world").unwrap();
        let entry = handle.commit().unwrap();
        assert_eq!(entry.name, "upload.bin");

        let final_path = dir.path().join("upload.bin");
        assert_eq!(std::fs::read(&final_path).unwrap(), b"hello world");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&final_path).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn write_handle_checks_collision_before_accepting_bytes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("taken.bin"), b"existing").unwrap();
        let header = WriteHeader {
            directory: dir.path().display().to_string(),
            name: "taken.bin".into(),
            overwrite: false,
            mode: None,
        };
        assert!(WriteHandle::open(&header).is_err());
        assert_eq!(
            std::fs::read(dir.path().join("taken.bin")).unwrap(),
            b"existing"
        );
    }

    #[test]
    fn set_attributes_changes_mode_without_touching_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let before = describe(&file).unwrap();

        let entry = set_attributes(&file.display().to_string(), Some(0o600), None, None).unwrap();
        assert_eq!(entry.mode, 0o600);
        // Neither owner name nor uid should move when owner/group are None.
        assert_eq!(entry.uid, before.uid);
        assert_eq!(entry.owner, before.owner);
    }

    #[test]
    fn set_attributes_rejects_an_unknown_user_or_group() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();

        let err = set_attributes(
            &file.display().to_string(),
            None,
            Some("definitely-not-a-real-user"),
            None,
        )
        .unwrap_err();
        assert!(matches!(err, FileError::UnknownUser(_)));

        let err = set_attributes(
            &file.display().to_string(),
            None,
            None,
            Some("definitely-not-a-real-group"),
        )
        .unwrap_err();
        assert!(matches!(err, FileError::UnknownGroup(_)));
    }

    #[test]
    fn set_attributes_refuses_a_protected_path() {
        let err = set_attributes("/etc", Some(0o755), None, None).unwrap_err();
        assert!(matches!(err, FileError::Protected(_)));
    }

    #[test]
    fn list_system_identities_includes_root() {
        let (users, groups) = list_system_identities().unwrap();
        let root = users.iter().find(|u| u.uid == 0).expect("root user exists");
        assert_eq!(root.name, "root");
        assert!(groups.iter().any(|g| g.gid == 0), "a gid-0 group exists");
        // Ascending by id, as documented.
        assert!(users.windows(2).all(|w| w[0].uid <= w[1].uid));
        assert!(groups.windows(2).all(|w| w[0].gid <= w[1].gid));
    }
}
