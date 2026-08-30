//! Path validation for [`super::FileService`] (DMN-070): the whole check
//! lives in construction, not confinement. There is no jail root — the scope
//! is the whole filesystem from `/` — so the job here is predictability, not
//! containment.

use std::path::{Component, Path, PathBuf};

use super::FileError;

/// Absolute path length cap (PATH_MAX on Linux, with headroom).
pub const MAX_PATH_BYTES: usize = 4096;
/// Path component length cap (NAME_MAX on Linux).
pub const MAX_COMPONENT_BYTES: usize = 255;

/// A path the daemon is willing to touch. Construction is the whole check:
/// absolute, no `..`, no NUL, within length limits, lexically normalized.
///
/// Deliberately NOT canonicalized: canonicalize resolves the final symlink,
/// and the whole policy of this service is that a symlink is a thing you
/// see, not a thing you go through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafePath(PathBuf);

impl SafePath {
    pub fn parse(raw: &str) -> Result<Self, FileError> {
        if raw.is_empty() || !raw.starts_with('/') {
            return Err(FileError::InvalidPath(format!(
                "path must be absolute: {raw:?}"
            )));
        }
        if raw.len() > MAX_PATH_BYTES {
            return Err(FileError::InvalidPath(format!(
                "path exceeds {MAX_PATH_BYTES} bytes"
            )));
        }
        if raw.contains('\0') {
            return Err(FileError::InvalidPath("path contains a NUL byte".into()));
        }
        let mut normalized = PathBuf::from("/");
        for component in Path::new(raw).components() {
            match component {
                // "//a" and "/./a" both normalize to "/a" — CurDir is a no-op.
                Component::RootDir | Component::CurDir => {}
                Component::Normal(part) => {
                    let part = part.to_string_lossy();
                    if part.len() > MAX_COMPONENT_BYTES {
                        return Err(FileError::InvalidPath(format!(
                            "path component exceeds {MAX_COMPONENT_BYTES} bytes: {part:?}"
                        )));
                    }
                    normalized.push(part.as_ref());
                }
                Component::ParentDir => {
                    return Err(FileError::InvalidPath(format!(
                        "path must not contain '..': {raw:?}"
                    )));
                }
                Component::Prefix(_) => {
                    unreachable!("an absolute unix path has no prefix component")
                }
            }
        }
        Ok(Self(normalized))
    }

    /// `self` joined with one path-separator-free component. Rejects a `name`
    /// containing a slash, `..`, `.`, NUL or an empty string — a name that
    /// comes from a browser upload must never be able to choose a directory.
    pub fn child(&self, name: &str) -> Result<Self, FileError> {
        if name.is_empty() || name == "." || name == ".." {
            return Err(FileError::InvalidPath(format!(
                "invalid file name: {name:?}"
            )));
        }
        if name.contains('/') || name.contains('\0') {
            return Err(FileError::InvalidPath(format!(
                "file name must not contain a path separator: {name:?}"
            )));
        }
        if name.len() > MAX_COMPONENT_BYTES {
            return Err(FileError::InvalidPath(format!(
                "file name exceeds {MAX_COMPONENT_BYTES} bytes"
            )));
        }
        Ok(Self(self.0.join(name)))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn parent(&self) -> Option<Self> {
        self.0.parent().map(|p| Self(p.to_path_buf()))
    }
}

impl std::fmt::Display for SafePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_normalizes_and_rejects_relative_and_traversal() {
        assert_eq!(
            SafePath::parse("/a/b").unwrap().as_path(),
            Path::new("/a/b")
        );
        assert_eq!(SafePath::parse("/").unwrap().as_path(), Path::new("/"));
        assert_eq!(
            SafePath::parse("//a///b/./c").unwrap().as_path(),
            Path::new("/a/b/c")
        );
        assert!(SafePath::parse("relative/path").is_err());
        assert!(SafePath::parse("").is_err());
        assert!(SafePath::parse("/a/../b").is_err());
        assert!(SafePath::parse("/..").is_err());
        assert!(SafePath::parse("/a\0b").is_err());
        assert!(SafePath::parse(&format!("/{}", "a".repeat(MAX_PATH_BYTES))).is_err());
    }

    #[test]
    fn parse_rejects_oversized_component() {
        let long = "a".repeat(MAX_COMPONENT_BYTES + 1);
        assert!(SafePath::parse(&format!("/{long}")).is_err());
        let ok = "a".repeat(MAX_COMPONENT_BYTES);
        assert!(SafePath::parse(&format!("/{ok}")).is_ok());
    }

    #[test]
    fn child_rejects_anything_that_is_not_a_bare_name() {
        let base = SafePath::parse("/etc/nginx").unwrap();
        assert!(base.child("app.conf").is_ok());
        assert!(base.child("../app.conf").is_err());
        assert!(base.child("sub/app.conf").is_err());
        assert!(base.child(".").is_err());
        assert!(base.child("..").is_err());
        assert!(base.child("").is_err());
        assert!(base.child("a\0b").is_err());
    }

    #[test]
    fn parent_of_root_is_none() {
        assert!(SafePath::parse("/").unwrap().parent().is_none());
        assert_eq!(
            SafePath::parse("/a/b").unwrap().parent().unwrap().as_path(),
            Path::new("/a")
        );
    }
}
