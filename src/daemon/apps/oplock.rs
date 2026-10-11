//! The per-app operation lock: `<app_dir>/.asc/op.lock`.
//!
//! An upgrade and a rebuild of the same app both rewrite its repository,
//! its images and its containers; two of them interleaving — or a service
//! settings write landing in the middle of one — would leave an app that is
//! neither version. Each of those operations therefore holds an exclusive,
//! non-blocking `flock(2)` on this file for its whole run: the second one is
//! refused at once ("another operation is in progress") rather than queued
//! behind a build that may take minutes. The lock lives in the kernel, so it
//! also covers the CLI running the same operation in-process next to the
//! daemon, and it is released by the kernel when the holder exits —
//! including a crash, so a stale lock file never blocks anything.

use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::Path;

use anyhow::{Context, Result};

use crate::daemon::i18n::{Msg, tf};

/// File name of the lock inside `.asc/`.
pub const LOCK_FILE: &str = "op.lock";

/// A held operation lock; dropping it (or the process ending) releases it.
#[derive(Debug)]
pub struct OpLock {
    _file: File,
}

/// Take the operation lock of the app in `app_dir`, or fail with a
/// precondition error naming `app` when another operation holds it.
pub fn acquire(app_dir: &Path, app: &str) -> Result<OpLock> {
    let state = super::layout::ensure_state_dir(app_dir)?;
    let path = state.join(LOCK_FILE);
    // Read-only is enough for flock(2), and it is all a regular user's CLI
    // can open once the root daemon has created the file.
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            File::open(&path).with_context(|| format!("cannot open {}", path.display()))?
        }
        Err(e) => return Err(e).with_context(|| format!("cannot open {}", path.display())),
    };
    // SAFETY: flock on a valid, owned descriptor has no other preconditions.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Err(crate::daemon::exec::precondition(tf(
                Msg::AppOperationInProgress,
                app,
            )));
        }
        return Err(err).with_context(|| format!("cannot lock {}", path.display()));
    }
    Ok(OpLock { _file: file })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_operation_is_refused_until_the_first_ends() {
        let dir = tempfile::tempdir().unwrap();
        let first = acquire(dir.path(), "demo").unwrap();
        let err = acquire(dir.path(), "demo").unwrap_err();
        assert!(
            matches!(
                err.downcast_ref::<crate::daemon::exec::ModuleError>(),
                Some(crate::daemon::exec::ModuleError::Precondition(_))
            ),
            "got: {err:#}"
        );
        drop(first);
        acquire(dir.path(), "demo").unwrap();
        assert!(dir.path().join(".asc").join(LOCK_FILE).exists());
    }

    #[test]
    fn locks_of_different_apps_are_independent() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let _a = acquire(a.path(), "a").unwrap();
        acquire(b.path(), "b").unwrap();
    }
}
