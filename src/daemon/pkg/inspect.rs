//! Read a package without installing it (DMN-098): clone the repository into
//! a temporary directory, read `asc.yaml` or `asc.stack.yaml` and throw the
//! clone away.
//!
//! What it is for: an installer UI cannot tell a stack from a single app
//! before the clone — the registry index only carries a `type` field for
//! packages that are in a registry at all, and a direct git install has no
//! entry whatsoever. The platform's install dialog calls this first, so an
//! operator about to install `cs2` sees that it is a stack and which apps it
//! is about to put on the node, instead of finding out from the result.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use super::install::{GitRef, git_clone};
use super::manifest::{Manifest, Requirements, StackManifest};
use crate::daemon::apps::UserContext;

/// What a package turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageKind {
    /// `asc.yaml` — one application.
    App,
    /// `asc.stack.yaml` — several applications shipped together.
    Stack,
    /// Neither manifest is present (DMN-106) — the repository may still be
    /// installable some other way, see [`PackageInfo::methods`].
    Unknown,
}

/// A package as its repository describes it.
#[derive(Debug, Clone)]
pub struct PackageInfo {
    pub kind: PackageKind,
    /// Package name: the app's own name, the stack's, or (kind `Unknown`) a
    /// best-effort guess from the repository URL.
    pub name: String,
    pub version: String,
    pub title: Option<String>,
    pub description: Option<String>,
    /// Declared minimum resources — apps only (a stack's requirements are its
    /// apps' own).
    pub requirements: Option<Requirements>,
    /// Stacks only: the apps the stack ships, in manifest order.
    pub apps: Vec<StackAppInfo>,
    /// Every installation method detected in the package directory (DMN-106),
    /// regardless of `kind` — a repository can carry `asc.yaml` next to a
    /// Dockerfile it doesn't need, and that is still worth reporting.
    pub methods: Vec<super::detect::DetectedMethod>,
}

/// One app of a stack, merged from `asc.stack.yaml` and the app's own
/// `asc.yaml`.
#[derive(Debug, Clone)]
pub struct StackAppInfo {
    /// Name within the stack (`asc install <stack>/<name>`, `--app <name>`).
    pub name: String,
    /// The id it installs under — the name from the app's own manifest, which
    /// is not the stack-local one (`server` → `cs2-server`).
    pub app_id: String,
    pub version: String,
    pub title: Option<String>,
    pub description: Option<String>,
    /// Skipped on a whole-stack install unless requested explicitly.
    pub optional: bool,
    /// Apps of the same stack installed and started before this one.
    pub depends_on: Vec<String>,
    pub requirements: Option<Requirements>,
}

/// Clone `url` (shallow, at `git_ref` when given) and read the package at
/// `path` inside it — the repository root when `path` is `None`. The clone is
/// removed before returning, whatever the outcome.
pub fn inspect_git(
    url: &str,
    git_ref: Option<GitRef<'_>>,
    path: Option<&str>,
    ctx: &UserContext,
) -> Result<PackageInfo> {
    let dir = std::env::temp_dir().join(format!(
        "asc-inspect-{}-{}",
        std::process::id(),
        // Two inspects of the same repository can run at once (two operators,
        // two nodes' dialogs): the nanosecond keeps them from sharing a path.
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or_default()
    ));
    let _ = fs::remove_dir_all(&dir);
    let cleanup = super::install::RemoveOnDrop {
        path: dir.clone(),
        armed: true,
    };
    let checkout = match git_ref {
        Some(GitRef::Branch(r)) | Some(GitRef::Tag(r)) => Some(r),
        None => None,
    };
    git_clone(url, checkout, &dir, ctx, None)?;
    let package_dir = super::install::manifest_dir(&dir, path)?;
    let info = read_package(&package_dir, url);
    drop(cleanup);
    info
}

/// Read whichever manifest the directory holds, stack first: a stack root may
/// not carry an `asc.yaml` of its own, an app directory never carries an
/// `asc.stack.yaml`. Neither present is no longer an error (DMN-106): the
/// repository may still be installable some other way (Dockerfile, compose,
/// …), so it comes back as `PackageKind::Unknown` with whatever
/// [`super::detect::detect`] found, rather than failing the whole inspect.
/// `url` is only used for that unknown-kind fallback name.
fn read_package(dir: &Path, url: &str) -> Result<PackageInfo> {
    let methods = super::detect::detect(dir);
    if dir.join(StackManifest::FILE).exists() {
        let stack = StackManifest::load(dir)?;
        let mut apps = Vec::with_capacity(stack.apps.len());
        for app in &stack.apps {
            let app_dir = super::install::safe_join(dir, &app.path)?;
            let manifest = Manifest::load(&app_dir).with_context(|| {
                format!("stack '{}': cannot read app '{}'", stack.name, app.name)
            })?;
            apps.push(StackAppInfo {
                name: app.name.clone(),
                app_id: manifest.name,
                version: manifest.version,
                title: manifest.title,
                description: manifest.description,
                optional: app.optional,
                depends_on: app.depends_on.clone(),
                requirements: manifest.requirements,
            });
        }
        return Ok(PackageInfo {
            kind: PackageKind::Stack,
            name: stack.name,
            version: stack.version,
            title: stack.title,
            description: stack.description,
            requirements: None,
            apps,
            methods,
        });
    }
    if dir.join(Manifest::FILE).exists() {
        let manifest = Manifest::load(dir)?;
        return Ok(PackageInfo {
            kind: PackageKind::App,
            name: manifest.name,
            version: manifest.version,
            title: manifest.title,
            description: manifest.description,
            requirements: manifest.requirements,
            apps: Vec::new(),
            methods,
        });
    }
    Ok(PackageInfo {
        kind: PackageKind::Unknown,
        name: super::install::repo_name(url).unwrap_or_default(),
        version: String::new(),
        title: None,
        description: None,
        requirements: None,
        apps: Vec::new(),
        methods,
    })
}
