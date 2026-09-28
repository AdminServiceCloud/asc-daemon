//! Read a package without installing it (DMN-098): snapshot the repository
//! into a temporary directory, read `asc.yaml` or `asc.stack.yaml` and throw
//! the snapshot away. The snapshot is sparse and blobless (DMN-131, see
//! [`super::sparse`]): only the manifests, the license and the files the
//! install-method detector reads are downloaded — never the package content.
//!
//! What it is for: an installer UI cannot tell a stack from a single app
//! before the clone — the registry index only carries a `type` field for
//! packages that are in a registry at all, and a direct git install has no
//! entry whatsoever. The platform's install dialog calls this first, so an
//! operator about to install `cs2` sees that it is a stack and which apps it
//! is about to put on the node, instead of finding out from the result.
//!
//! The same read answers the two questions an install used to discover only
//! by failing (DMN-131): does the repository ship a license the operator has
//! to accept, and can this host cover the package's declared requirements
//! right now. The installer asks both before it starts the one real install.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::debug;

use super::install::{GitRef, load_quota, repo_license, safe_join};
use super::manifest::{Manifest, Requirements, StackManifest};
use super::resources::{self, RequirementsNotMet};
use super::settings::SettingsFile;
use crate::daemon::apps::UserContext;
use crate::daemon::monitor;

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
    /// The license text an install would ask consent for (DMN-131) — the
    /// package directory's LICENSE*, else the repository root's.
    pub license: Option<String>,
    /// What this host falls short on for the package (DMN-131): the first
    /// app (the app itself, or a non-optional member of a stack) whose
    /// requirements or runtime quota the host cannot cover right now. `None`
    /// when everything fits or the metrics could not be read.
    pub shortfall: Option<RequirementsNotMet>,
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

/// Snapshot `url` (sparse and blobless, at `git_ref` when given) and read the
/// package at `path` inside it — the repository root when `path` is `None`.
/// `apps_dir` is where an install would put the app: its filesystem is the
/// one the disk requirement is checked against. The snapshot is removed
/// before returning, whatever the outcome.
pub fn inspect_git(
    url: &str,
    git_ref: Option<GitRef<'_>>,
    path: Option<&str>,
    ctx: &UserContext,
    apps_dir: &Path,
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
    super::sparse::sparse_clone(url, checkout, path, &dir, ctx)?;
    let package_dir = super::install::manifest_dir(&dir, path)?;
    fetch_custom_settings(&dir, &package_dir, url, ctx);
    let mut info = read_package(&package_dir, url)?;
    info.license = repo_license(&package_dir, &dir);
    info.shortfall = preflight_resources(&package_dir, &info, apps_dir);
    drop(cleanup);
    Ok(info)
}

/// Every app manifest of the package: the app itself, or the stack's
/// members, as `(app id, manifest directory, optional)`.
fn app_manifests(package_dir: &Path) -> Vec<(String, PathBuf, bool)> {
    if package_dir.join(StackManifest::FILE).exists() {
        let Ok(stack) = StackManifest::load(package_dir) else {
            return Vec::new();
        };
        return stack
            .apps
            .iter()
            .filter_map(|app| {
                let dir = safe_join(package_dir, &app.path).ok()?;
                let manifest = Manifest::load(&dir).ok()?;
                Some((manifest.name, dir, app.optional))
            })
            .collect();
    }
    match Manifest::load(package_dir) {
        Ok(manifest) => vec![(manifest.name, package_dir.to_path_buf(), false)],
        Err(_) => Vec::new(),
    }
}

/// A manifest may keep its settings under a non-default name (`settings:
/// config/schema.yaml`); the snapshot only has `asc.settings.yaml` checked
/// out, so fetch whatever else the manifests name. Best effort: a settings
/// file that cannot be fetched only costs the quota part of the preflight.
fn fetch_custom_settings(repo: &Path, package_dir: &Path, url: &str, ctx: &UserContext) {
    let mut missing = Vec::new();
    for (_, dir, _) in app_manifests(package_dir) {
        let Ok(manifest) = Manifest::load(&dir) else {
            continue;
        };
        let Some(rel) = manifest.settings.as_deref() else {
            continue;
        };
        let Ok(path) = safe_join(&dir, rel) else {
            continue;
        };
        if path.exists() {
            continue;
        }
        if let Ok(repo_rel) = path.strip_prefix(repo) {
            missing.push(repo_rel.to_string_lossy().replace('\\', "/"));
        }
    }
    if let Err(err) = super::sparse::sparse_add(repo, &missing, url, ctx) {
        debug!(error = %format!("{err:#}"), "cannot fetch custom settings files");
    }
}

/// The resource preflight (DMN-131) — the same `resources::check` an install
/// runs after its clone, run before it instead. A stack is checked member by
/// member (non-optional ones — what a whole-stack install puts on the node)
/// and reports the first member that does not fit.
fn preflight_resources(
    package_dir: &Path,
    info: &PackageInfo,
    apps_dir: &Path,
) -> Option<RequirementsNotMet> {
    if info.kind == PackageKind::Unknown {
        return None;
    }
    let metrics = match monitor::system::snapshot_blocking() {
        Ok(metrics) => metrics,
        Err(err) => {
            debug!(error = %format!("{err:#}"), "cannot read system metrics; skipping the preflight");
            return None;
        }
    };
    // No app directory exists yet, so there are no operator overrides of the
    // quota to merge: a path that is never there reads as "none".
    let no_config = apps_dir.join(".asc-preflight-no-config");
    for (app, dir, optional) in app_manifests(package_dir) {
        if optional {
            continue;
        }
        let Ok(manifest) = Manifest::load(&dir) else {
            continue;
        };
        let settings = SettingsFile::load_for(&dir, &manifest).ok().flatten();
        let quota = load_quota(settings.as_ref(), &no_config).ok().flatten();
        let shortages = resources::check(
            manifest.requirements.as_ref(),
            quota.as_ref(),
            &metrics,
            apps_dir,
        );
        if !shortages.is_empty() {
            return Some(RequirementsNotMet { app, shortages });
        }
    }
    None
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
            license: None,
            shortfall: None,
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
            license: None,
            shortfall: None,
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
        license: None,
        shortfall: None,
    })
}
