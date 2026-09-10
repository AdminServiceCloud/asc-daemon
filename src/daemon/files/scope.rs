//! App-scoped confinement for [`super::FileService`]-style operations
//! (DMN-086): unlike the unscoped node file manager, whose whole policy is
//! "no jail root" (see [`super::path`]), an [`AppScope`] *is* a jail — every
//! path a caller supplies must resolve inside one of its roots or the
//! operation is refused, no matter what root-equivalent context the call
//! arrived with.
//!
//! This matters because the TCP transport (the platform) always presents a
//! full-rights [`crate::daemon::apps::UserContext`]: the platform checks the
//! calling user's own permissions (`apps.edit` vs `files.edit`) before ever
//! reaching the daemon. A user with `apps.edit` but not `files.edit` must
//! still be unable to read `/etc/shadow` through this service — see
//! `asc-platform/docs/features/app-file-manager.md`. Confinement therefore
//! has to be enforced here, not trusted from the caller.

use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::daemon::apps::{AppMeta, AppStore};
use crate::daemon::config::Config;
use crate::daemon::pkg::manifest::Manifest;
use crate::daemon::pkg::settings;

use super::path::SafePath;
use super::{FileError, Result};

/// A set of confinement roots, each already canonicalized (symlinks
/// resolved) at construction time. Built once per request from an app's
/// directory and private volumes — see
/// [`crate::daemon::apps::disk::private_volume_roots`].
#[derive(Debug, Clone)]
pub struct AppScope {
    roots: Vec<PathBuf>,
}

impl AppScope {
    /// Canonicalizes every root so later containment checks compare real
    /// filesystem locations, not names a symlink could redirect. A root that
    /// does not exist (or cannot be resolved) fails the whole scope — an app
    /// file manager with a missing root is a bug, not something to silently
    /// narrow.
    pub fn new(roots: Vec<PathBuf>) -> Result<Self> {
        let mut real_roots: Vec<PathBuf> = Vec::with_capacity(roots.len());
        for root in roots {
            let real = std::fs::canonicalize(&root).map_err(|e| FileError::Io(root.clone(), e))?;
            if !real_roots.contains(&real) {
                real_roots.push(real);
            }
        }
        Ok(Self { roots: real_roots })
    }

    /// The scope for one installed app (DMN-086): its own directory plus
    /// every private (non-shared) volume, mirroring what
    /// `app-file-manager.md` calls "the app directory and its non-public
    /// volumes". A manifest that cannot be located or parsed degrades to
    /// just the app directory — the same directory `GetAppDisk` always
    /// reports — rather than failing the whole file manager over a broken
    /// registry link.
    pub fn for_app(config: &Config, store: &AppStore, meta: &AppMeta) -> anyhow::Result<Self> {
        let app_dir = store.app_dir(&meta.id)?;
        let mut roots = vec![app_dir.clone()];
        if let Ok((manifest_dir, _)) = settings::locate_installed(config, meta, &app_dir)
            && let Ok(manifest) = Manifest::load(&manifest_dir)
        {
            roots.extend(crate::daemon::apps::disk::private_volume_roots(
                &app_dir,
                &manifest_dir,
                &manifest,
            ));
        }
        Self::new(roots).context("cannot build the app's file-manager scope")
    }

    fn contains(&self, real: &Path) -> bool {
        self.roots
            .iter()
            .any(|root| real == root || real.starts_with(root))
    }

    /// Resolve `safe` to its real, symlink-free location and confirm it
    /// falls under one of this scope's roots.
    ///
    /// The deepest already-existing ancestor of `safe` is canonicalized —
    /// dereferencing every symlink along the way, including one planted
    /// partway down a path that otherwise looks like it stays inside the
    /// scope (e.g. `data/escape -> /etc`, then a request for
    /// `data/escape/shadow`). Any remaining, not-yet-existing tail is
    /// appended literally: it cannot itself be a symlink, because nothing is
    /// there yet.
    pub fn resolve(&self, safe: &SafePath) -> Result<PathBuf> {
        let real = canonicalize_existing_prefix(safe.as_path())?;
        if self.contains(&real) {
            Ok(real)
        } else {
            Err(FileError::OutsideScope(safe.as_path().to_path_buf()))
        }
    }
}

fn canonicalize_existing_prefix(path: &Path) -> Result<PathBuf> {
    match std::fs::canonicalize(path) {
        Ok(real) => Ok(real),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .ok_or_else(|| FileError::Io(path.to_path_buf(), e))?;
            let name = path.file_name().ok_or_else(|| {
                FileError::InvalidPath(format!("path has no file name: {path:?}"))
            })?;
            Ok(canonicalize_existing_prefix(parent)?.join(name))
        }
        Err(e) => Err(FileError::Io(path.to_path_buf(), e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_allows_a_path_inside_the_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("data")).unwrap();
        let scope = AppScope::new(vec![dir.path().to_path_buf()]).unwrap();

        let safe = SafePath::parse(&dir.path().join("data").display().to_string()).unwrap();
        let real = scope.resolve(&safe).unwrap();
        assert_eq!(
            real,
            std::fs::canonicalize(dir.path().join("data")).unwrap()
        );
    }

    #[test]
    fn resolve_allows_a_not_yet_existing_path_inside_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let scope = AppScope::new(vec![dir.path().to_path_buf()]).unwrap();

        let safe =
            SafePath::parse(&dir.path().join("new/deep/file.txt").display().to_string()).unwrap();
        let real = scope.resolve(&safe).unwrap();
        assert_eq!(
            real,
            std::fs::canonicalize(dir.path())
                .unwrap()
                .join("new/deep/file.txt")
        );
    }

    #[test]
    fn resolve_rejects_a_path_outside_every_root() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let scope = AppScope::new(vec![dir.path().to_path_buf()]).unwrap();

        let safe = SafePath::parse(&outside.path().join("secret").display().to_string()).unwrap();
        let err = scope.resolve(&safe).unwrap_err();
        assert!(matches!(err, FileError::OutsideScope(_)));
    }

    #[test]
    fn resolve_rejects_escape_through_a_symlink_planted_inside_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("shadow"), b"root:x:0:0").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).unwrap();
        let scope = AppScope::new(vec![dir.path().to_path_buf()]).unwrap();

        let safe =
            SafePath::parse(&dir.path().join("escape/shadow").display().to_string()).unwrap();
        let err = scope.resolve(&safe).unwrap_err();
        assert!(
            matches!(err, FileError::OutsideScope(_)),
            "a symlink inside the scope must not be a way out of it"
        );
    }

    #[test]
    fn resolve_rejects_escape_through_a_symlink_with_a_not_yet_existing_tail() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).unwrap();
        let scope = AppScope::new(vec![dir.path().to_path_buf()]).unwrap();

        // "escape/newfile.txt" does not exist yet, but its parent
        // ("escape") is a symlink out of scope — must still be rejected.
        let safe =
            SafePath::parse(&dir.path().join("escape/newfile.txt").display().to_string()).unwrap();
        let err = scope.resolve(&safe).unwrap_err();
        assert!(matches!(err, FileError::OutsideScope(_)));
    }

    #[test]
    fn resolve_allows_the_root_itself() {
        let dir = tempfile::tempdir().unwrap();
        let scope = AppScope::new(vec![dir.path().to_path_buf()]).unwrap();
        let safe = SafePath::parse(&dir.path().display().to_string()).unwrap();
        assert!(scope.resolve(&safe).is_ok());
    }

    #[test]
    fn new_accepts_multiple_roots_and_deduplicates() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let scope = AppScope::new(vec![
            a.path().to_path_buf(),
            b.path().to_path_buf(),
            a.path().to_path_buf(),
        ])
        .unwrap();
        let safe_a = SafePath::parse(&a.path().display().to_string()).unwrap();
        let safe_b = SafePath::parse(&b.path().display().to_string()).unwrap();
        assert!(scope.resolve(&safe_a).is_ok());
        assert!(scope.resolve(&safe_b).is_ok());
    }

    #[test]
    fn new_fails_when_a_root_does_not_exist() {
        let missing = std::env::temp_dir().join("asc-app-scope-test-missing-root");
        let _ = std::fs::remove_dir(&missing);
        assert!(AppScope::new(vec![missing]).is_err());
    }
}
