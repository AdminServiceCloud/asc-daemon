//! inotify watcher of the app index (Linux): notices changes made around
//! the daemon — a hand-edited `settings.json`, an app directory restored by
//! hand, a CLI of an older version — and re-syncs the one app they touch.
//!
//! Every watch is non-recursive, so the heavy trees of an app
//! (`repository/`, `data/`) are never watched:
//!
//! | Directory | Events of interest |
//! |---|---|
//! | apps root | an app directory appearing or disappearing |
//! | `<root>/<id>` | `.asc`, the legacy `meta.json` and `config`, `repository` |
//! | `<root>/<id>/.asc` | any state file written, renamed into place or removed |
//! | `<root>/<id>/repository/.git` | `HEAD` and `packed-refs` (the checked-out commit) |
//!
//! Events are coalesced for [`DEBOUNCE`] and each touched app is synced
//! once, through [`AppIndex::sync_if_stale`] — the daemon's own writes are
//! already in the index and cost only a `stat` here. A queue overflow
//! falls back to a full verify. Whatever the watcher misses (a watch limit,
//! an app directory replaced between two events) the periodic verify still
//! catches.

use std::collections::{BTreeSet, HashMap};
use std::ffi::{CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{debug, warn};

use super::AppIndex;
use crate::daemon::apps::meta::validate_id;

/// How long events are gathered before the touched apps are synced: an
/// install writes several files in a row, and one sync covers them all.
const DEBOUNCE: Duration = Duration::from_millis(200);

/// What a watch descriptor watches.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Watched {
    Root,
    App(String),
    State(String),
    Git(String),
}

const ROOT_MASK: u32 =
    libc::IN_CREATE | libc::IN_DELETE | libc::IN_MOVED_FROM | libc::IN_MOVED_TO | libc::IN_ONLYDIR;
const APP_MASK: u32 = libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_CLOSE_WRITE
    | libc::IN_ONLYDIR;
const STATE_MASK: u32 = libc::IN_CLOSE_WRITE
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_DELETE
    | libc::IN_ONLYDIR;
const GIT_MASK: u32 = libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO | libc::IN_DELETE | libc::IN_ONLYDIR;

/// Names inside an app directory that matter to the index.
const APP_ENTRIES: [&str; 4] = [".asc", "meta.json", "config", "repository"];
/// Names inside `.git` that move the checked-out commit.
const GIT_ENTRIES: [&str; 2] = ["HEAD", "packed-refs"];

struct Watcher {
    fd: i32,
    root: PathBuf,
    by_wd: HashMap<i32, Watched>,
    /// Logged once: the per-user inotify watch limit is shared with every
    /// other program on the host.
    limit_warned: bool,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // SAFETY: fd is an inotify descriptor this struct owns.
        unsafe { libc::close(self.fd) };
    }
}

impl Watcher {
    fn new(root: &Path) -> Result<Self> {
        // SAFETY: no pointers involved; the result is checked.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("inotify_init1");
        }
        Ok(Self {
            fd,
            root: root.to_path_buf(),
            by_wd: HashMap::new(),
            limit_warned: false,
        })
    }

    fn add(&mut self, path: &Path, mask: u32, what: Watched) -> bool {
        let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) else {
            return false;
        };
        // SAFETY: c_path is a valid NUL-terminated string for the call.
        let wd = unsafe { libc::inotify_add_watch(self.fd, c_path.as_ptr(), mask) };
        if wd < 0 {
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                // Not there (yet): the parent's watch reports it appearing.
                Some(libc::ENOENT) | Some(libc::ENOTDIR) => {}
                Some(libc::ENOSPC) if !self.limit_warned => {
                    self.limit_warned = true;
                    warn!(path = %path.display(), "inotify watch limit reached (fs.inotify.max_user_watches); the app index relies on its periodic verify for the rest");
                }
                _ => debug!(path = %path.display(), error = %err, "cannot watch for the app index"),
            }
            return false;
        }
        // The same inode watched twice returns the same descriptor; the
        // latest meaning wins, which is the same meaning.
        self.by_wd.insert(wd, what);
        true
    }

    fn watch_root(&mut self) -> bool {
        let root = self.root.clone();
        self.add(&root, ROOT_MASK, Watched::Root)
    }

    fn watch_app(&mut self, id: &str) {
        let dir = self.root.join(id);
        if !self.add(&dir, APP_MASK, Watched::App(id.to_string())) {
            return;
        }
        self.watch_state(id);
        self.watch_git(id);
    }

    fn watch_state(&mut self, id: &str) {
        let dir = crate::daemon::apps::layout::state_dir(&self.root.join(id));
        self.add(&dir, STATE_MASK, Watched::State(id.to_string()));
    }

    fn watch_git(&mut self, id: &str) {
        let dir = self.root.join(id).join("repository").join(".git");
        self.add(&dir, GIT_MASK, Watched::Git(id.to_string()));
    }

    fn watch_everything(&mut self) {
        if !self.watch_root() {
            return;
        }
        for id in super::scan::app_ids(&self.root).unwrap_or_default() {
            self.watch_app(&id);
        }
    }

    /// Wait up to `timeout` for events; the apps they touch go into
    /// `dirty`. `true` when the kernel queue overflowed.
    fn poll(&mut self, timeout: Duration, dirty: &mut BTreeSet<String>) -> bool {
        let mut pfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: pfd is a valid pollfd for one descriptor.
        let ready = unsafe { libc::poll(&mut pfd, 1, millis) };
        if ready <= 0 {
            return false;
        }
        let mut overflow = false;
        // Large enough for many events per read; inotify never splits one.
        let mut buf = [0u8; 16 * 1024];
        loop {
            // SAFETY: buf is valid for writes of its length.
            let n = unsafe { libc::read(self.fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
            let mut offset = 0usize;
            let header = std::mem::size_of::<libc::inotify_event>();
            while offset + header <= n as usize {
                // SAFETY: the kernel wrote a whole inotify_event at offset;
                // read_unaligned copes with the byte buffer's alignment.
                let event: libc::inotify_event =
                    unsafe { std::ptr::read_unaligned(buf.as_ptr().add(offset).cast()) };
                let name_start = offset + header;
                let name_end = (name_start + event.len as usize).min(n as usize);
                let raw = &buf[name_start..name_end];
                let name = raw.split(|b| *b == 0).next().unwrap_or_default();
                offset = name_end;
                if event.mask & libc::IN_Q_OVERFLOW != 0 {
                    overflow = true;
                    continue;
                }
                self.handle(event.wd, event.mask, OsStr::from_bytes(name), dirty);
            }
        }
        overflow
    }

    fn handle(&mut self, wd: i32, mask: u32, name: &OsStr, dirty: &mut BTreeSet<String>) {
        if mask & libc::IN_IGNORED != 0 {
            // The watched directory is gone; its parent reports why.
            self.by_wd.remove(&wd);
            return;
        }
        let Some(what) = self.by_wd.get(&wd).cloned() else {
            return;
        };
        let name = name.to_str().unwrap_or_default();
        match what {
            Watched::Root => {
                if validate_id(name).is_err() {
                    return;
                }
                if mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0 {
                    self.watch_app(name);
                }
                dirty.insert(name.to_string());
            }
            Watched::App(id) => {
                if !APP_ENTRIES.contains(&name) {
                    return;
                }
                if mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0 {
                    match name {
                        ".asc" => self.watch_state(&id),
                        "repository" => self.watch_git(&id),
                        _ => {}
                    }
                }
                dirty.insert(id);
            }
            Watched::State(id) => {
                if super::scan::mirrored_name(name) && mask & libc::IN_ISDIR == 0 {
                    dirty.insert(id);
                }
            }
            Watched::Git(id) => {
                if GIT_ENTRIES.contains(&name) {
                    dirty.insert(id);
                }
            }
        }
    }
}

/// Start the watcher thread for `index`. Fails only when inotify itself is
/// unavailable; a root that does not exist yet is watched once it appears
/// (checked on every idle second).
pub fn spawn(index: Arc<AppIndex>) -> Result<()> {
    let mut watcher = Watcher::new(index.root())?;
    watcher.watch_everything();
    std::thread::Builder::new()
        .name("asc-app-index-watch".into())
        .spawn(move || run(index, watcher))
        .context("cannot start the app index watcher")?;
    Ok(())
}

fn run(index: Arc<AppIndex>, mut watcher: Watcher) {
    let mut dirty = BTreeSet::new();
    let mut first_event: Option<Instant> = None;
    loop {
        if !watcher.by_wd.values().any(|w| *w == Watched::Root) && watcher.watch_root() {
            // The root appeared (first install on a fresh host).
            watcher.watch_everything();
            let _ = index.verify();
        }
        let timeout = match first_event {
            Some(at) => DEBOUNCE.saturating_sub(at.elapsed()),
            None => Duration::from_secs(1),
        };
        let overflow = watcher.poll(timeout, &mut dirty);
        if overflow {
            warn!("app index watcher queue overflowed, verifying all apps");
            dirty.clear();
            first_event = None;
            watcher.by_wd.clear();
            watcher.watch_everything();
            if let Err(err) = index.verify() {
                warn!(error = %format!("{err:#}"), "app index verify failed");
            }
            continue;
        }
        if !dirty.is_empty() && first_event.is_none() {
            first_event = Some(Instant::now());
        }
        let due = first_event.is_some_and(|at| at.elapsed() >= DEBOUNCE);
        if !due {
            continue;
        }
        for id in std::mem::take(&mut dirty) {
            match index.sync_if_stale(&id) {
                Ok(outcome) => debug!(app = %id, ?outcome, "app index watcher sync"),
                Err(err) => {
                    warn!(app = %id, error = %format!("{err:#}"), "app index watcher could not sync the app")
                }
            }
        }
        first_event = None;
    }
}
