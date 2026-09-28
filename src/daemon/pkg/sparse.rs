//! Lightweight package snapshot (DMN-131): read what a package repository
//! declares without downloading its content.
//!
//! Inspecting a package — "is it an app or a stack, what does it need, does it
//! ship a license?" — only ever reads a handful of small files: `asc.yaml`,
//! `asc.stack.yaml`, `asc.settings.yaml`, `LICENSE*` and the few files the
//! install-method detector looks at (compose files, Dockerfiles, Kubernetes
//! YAML, `Chart.yaml`). A plain shallow clone still downloads every blob of
//! the tip commit — for a game server that ships assets that is hundreds of
//! megabytes, per click of the installer dialog.
//!
//! So the snapshot is a *blobless, sparse* clone:
//!
//! 1. `git clone --depth 1 --filter=blob:none --no-checkout` fetches the
//!    commit and its trees only (a directory listing, no file contents);
//! 2. `core.sparseCheckout` with `.git/info/sparse-checkout` limits the
//!    working tree to the patterns below;
//! 3. `git read-tree -mu HEAD` checks those paths out, and git fetches just
//!    their blobs from the promisor remote in one batch.
//!
//! The result is an ordinary directory, so [`super::detect::detect`] and the
//! manifest readers work on it unchanged. `core.sparseCheckout` + the info
//! file is used instead of `git sparse-checkout set --no-cone` on purpose:
//! it has been there since git 1.7, while `--no-cone` needs 2.35 and the
//! target distributions (Debian 11, Ubuntu 20.04) ship older gits.
//!
//! A server without partial-clone support ignores the filter (git prints
//! "filtering not recognized by server, ignoring") and sends the blobs
//! anyway: the snapshot degrades to the old shallow clone, never fails
//! because of it.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use tracing::{debug, warn};

use super::install::git_clone_with;
use crate::daemon::apps::UserContext;
use crate::daemon::i18n::{Msg, t};

/// Deepest directory level (below the package directory) whose YAML files the
/// snapshot checks out — the install-method detector's walk depth
/// ([`super::detect`]), so Kubernetes manifests it would find in a full clone
/// are found here too.
const YAML_DEPTH: usize = 3;

/// Files the snapshot checks out wherever they are in the repository: small
/// by nature and needed wherever a stack keeps its members.
const ANYWHERE: &[&str] = &[
    "asc.yaml",
    "asc.stack.yaml",
    "asc.settings.yaml",
    "Dockerfile*",
    "compose.yml",
    "compose.yaml",
    "docker-compose.yml",
    "docker-compose.yaml",
    "docker-stack.yml",
    "docker-stack.yaml",
    "Chart.yaml",
];

const LICENSE_FILES: &[&str] = &["LICENSE", "LICENSE.md", "LICENSE.txt"];

/// The sparse-checkout patterns for a package at `path` (the repository root
/// when `None`). Patterns follow `.gitignore` syntax: a leading `/` anchors
/// at the repository root, a bare name matches at any depth.
pub(super) fn package_patterns(path: Option<&str>) -> Vec<String> {
    let base = match path
        .map(|p| p.trim().trim_matches('/'))
        .filter(|p| !p.is_empty())
    {
        Some(path) => format!("/{path}/"),
        None => "/".to_string(),
    };
    let mut patterns: Vec<String> = ANYWHERE.iter().map(|p| (*p).to_string()).collect();
    // The license of the package directory wins, the repository root is the
    // fallback — same lookup order as `install::repo_license`.
    for name in LICENSE_FILES {
        patterns.push(format!("/{name}"));
        if base != "/" {
            patterns.push(format!("{base}{name}"));
        }
    }
    for depth in 0..=YAML_DEPTH {
        let dirs = "*/".repeat(depth);
        patterns.push(format!("{base}{dirs}*.yml"));
        patterns.push(format!("{base}{dirs}*.yaml"));
    }
    patterns
}

/// Snapshot the repository at `checkout` (branch or tag; `None` — the default
/// branch HEAD) into `dest`, with the working tree limited to
/// [`package_patterns`]. `dest` must not exist yet.
pub(super) fn sparse_clone(
    url: &str,
    checkout: Option<&str>,
    path: Option<&str>,
    dest: &Path,
    ctx: &UserContext,
) -> Result<()> {
    git_clone_with(
        url,
        checkout,
        dest,
        ctx,
        None,
        &["--filter=blob:none", "--no-checkout"],
    )?;
    git_in(dest, &["config", "core.sparseCheckout", "true"], url, ctx)?;
    // Newer gits default `git sparse-checkout` to cone mode; the patterns
    // here are full .gitignore-style ones, so say so explicitly.
    git_in(
        dest,
        &["config", "core.sparseCheckoutCone", "false"],
        url,
        ctx,
    )?;
    write_patterns(dest, &package_patterns(path))?;
    git_in(dest, &["read-tree", "-mu", "HEAD"], url, ctx)?;
    Ok(())
}

/// Widen an existing snapshot by `paths` (repository-relative files, e.g. a
/// settings file a manifest names under a non-default path) and check them
/// out — fetching only their blobs.
pub(super) fn sparse_add(
    dest: &Path,
    paths: &[String],
    url: &str,
    ctx: &UserContext,
) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let file = dest.join(".git").join("info").join("sparse-checkout");
    let mut current = fs::read_to_string(&file).unwrap_or_default();
    for path in paths {
        current.push('/');
        current.push_str(path.trim_start_matches('/'));
        current.push('\n');
    }
    fs::write(&file, current).with_context(|| format!("cannot write {}", file.display()))?;
    git_in(dest, &["read-tree", "-mu", "HEAD"], url, ctx)
}

fn write_patterns(dest: &Path, patterns: &[String]) -> Result<()> {
    let info = dest.join(".git").join("info");
    fs::create_dir_all(&info).with_context(|| format!("cannot create {}", info.display()))?;
    let mut body = patterns.join("\n");
    body.push('\n');
    let file = info.join("sparse-checkout");
    fs::write(&file, body).with_context(|| format!("cannot write {}", file.display()))
}

/// Run `git -C <dir> <args>` with the same credential the clone used: a
/// blobless checkout goes back to the remote for the blobs it needs, and a
/// private repository wants the token or key again for that.
fn git_in(dir: &Path, args: &[&str], url: &str, ctx: &UserContext) -> Result<()> {
    let auth = match super::auth::GitAuth::load_for(ctx) {
        Ok(auth) => Some(auth),
        Err(err) => {
            warn!(error = %format!("{err:#}"), "cannot read git credentials, fetching without auth");
            None
        }
    };
    let credential = auth.as_ref().and_then(|a| a.lookup(url));
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::piped());
    let _askpass = super::auth::configure_git(&mut cmd, credential.map(|c| &c.method))?;
    let output = match cmd.output() {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(t(Msg::ErrGitNotFound)),
        Err(e) => return Err(e).context("cannot run git"),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }
    debug!(dir = %dir.display(), args = ?args, "git ok");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git must be installed to run this test");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// The snapshot checks out the manifests and the license, leaves the
    /// package content out of the working tree and — over a transport that
    /// supports partial clone — never downloads its blobs at all.
    #[test]
    fn sparse_clone_fetches_manifests_but_not_content() {
        if Command::new("git").arg("--version").output().is_err() {
            eprintln!("skipping: git is not available");
            return;
        }
        let ws = tempfile::tempdir().unwrap();
        let work = ws.path().join("work");
        fs::create_dir_all(work.join("assets")).unwrap();
        fs::create_dir_all(work.join("k8s")).unwrap();
        fs::write(
            work.join("asc.yaml"),
            "name: demo
version: 1.0.0
",
        )
        .unwrap();
        fs::write(
            work.join("asc.settings.yaml"),
            "settings: []
",
        )
        .unwrap();
        fs::write(
            work.join("LICENSE"),
            "MIT
",
        )
        .unwrap();
        fs::write(
            work.join("Dockerfile"),
            "FROM scratch
",
        )
        .unwrap();
        fs::write(
            work.join("k8s/deploy.yaml"),
            "apiVersion: v1
kind: Service
",
        )
        .unwrap();
        fs::write(work.join("assets/big.bin"), vec![7u8; 256 * 1024]).unwrap();
        fs::write(
            work.join("main.go"),
            "package main
",
        )
        .unwrap();
        git(&work, &["init", "-q"]);
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "init"]);
        let bare = ws.path().join("bare.git");
        git(
            ws.path(),
            &[
                "clone",
                "-q",
                "--bare",
                work.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        git(&bare, &["config", "uploadpack.allowFilter", "true"]);
        let big_blob = git(&work, &["rev-parse", "HEAD:assets/big.bin"])
            .trim()
            .to_string();

        let dest = ws.path().join("snapshot");
        let url = format!("file://{}", bare.display());
        sparse_clone(&url, None, None, &dest, &UserContext::current()).unwrap();

        for present in [
            "asc.yaml",
            "asc.settings.yaml",
            "LICENSE",
            "Dockerfile",
            "k8s/deploy.yaml",
        ] {
            assert!(
                dest.join(present).is_file(),
                "{present} must be checked out"
            );
        }
        for absent in ["assets/big.bin", "main.go"] {
            assert!(
                !dest.join(absent).exists(),
                "{absent} must stay out of the working tree"
            );
        }
        let missing = git(&dest, &["rev-list", "--objects", "--missing=print", "HEAD"]);
        assert!(
            missing
                .lines()
                .any(|line| line.trim_start_matches('?') == big_blob && line.starts_with('?')),
            "the asset blob must not be downloaded:
{missing}"
        );

        // A settings file under a custom path is fetched on demand.
        sparse_add(
            &dest,
            &["main.go".to_string()],
            &url,
            &UserContext::current(),
        )
        .unwrap();
        assert!(dest.join("main.go").is_file());
    }

    #[test]
    fn root_patterns_anchor_licenses_and_yaml_at_the_root() {
        let patterns = package_patterns(None);
        assert!(patterns.contains(&"asc.yaml".to_string()));
        assert!(patterns.contains(&"/LICENSE".to_string()));
        assert!(patterns.contains(&"/*.yml".to_string()));
        assert!(patterns.contains(&"/*/*/*/*.yaml".to_string()));
        assert!(!patterns.iter().any(|p| p.starts_with("//")));
    }

    #[test]
    fn subdirectory_patterns_cover_the_package_and_the_root_license() {
        let patterns = package_patterns(Some("/web/helloworld/"));
        assert!(patterns.contains(&"/LICENSE.md".to_string()));
        assert!(patterns.contains(&"/web/helloworld/LICENSE.md".to_string()));
        assert!(patterns.contains(&"/web/helloworld/*.yml".to_string()));
        assert!(patterns.contains(&"/web/helloworld/*/*/*/*.yml".to_string()));
        assert!(!patterns.contains(&"/*.yml".to_string()));
    }
}
