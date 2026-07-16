//! End-to-end direct git install (`asc install <url>`, DMN-040): no registry
//! involved at all. Runs as a separate test binary, mirroring `install.rs`.

use std::fs;
use std::path::Path;
use std::process::Command;

use asc_daemon::daemon::apps::{AppStore, UserContext};
use asc_daemon::daemon::config::Config;
use asc_daemon::daemon::pkg::{self, GitRef};

fn git(dir: &Path, args: &[&str]) {
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
}

#[test]
fn install_direct_from_git_url() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: git is not available");
        return;
    }
    let ws = tempfile::tempdir().unwrap();

    // A repository with asc.yaml at the root, its default branch, a `dev`
    // branch with different content, and a tag — everything `--branch`/
    // `--tag` need to pick between.
    let repo = ws.path().join("demo");
    fs::create_dir_all(&repo).unwrap();
    fs::write(
        repo.join("asc.yaml"),
        "name: demo\nversion: 1.0.0\ntype: native\nruntime:\n  start: ./run.sh\n",
    )
    .unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let default_branch = String::from_utf8(
        Command::new("git")
            .args(["branch", "--show-current"])
            .current_dir(&repo)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    git(&repo, &["tag", "v1.0.0"]);

    git(&repo, &["checkout", "-q", "-b", "dev"]);
    fs::write(
        repo.join("asc.yaml"),
        "name: demo\nversion: 1.1.0-dev\ntype: native\nruntime:\n  start: ./run.sh\n",
    )
    .unwrap();
    git(&repo, &["commit", "-q", "-am", "dev changes"]);
    git(&repo, &["checkout", "-q", &default_branch]);

    let url = repo.display().to_string().replace('\\', "/");

    let mut config = Config::default();
    config.daemon.data_dir = ws.path().join("data");
    config.daemon.apps_dir = ws.path().join("apps");
    let ctx = UserContext {
        uid: 1000,
        name: "tester".into(),
        is_root: false,
    };
    let store = AppStore::new(config.daemon.apps_dir.clone());

    // No --branch/--tag: clones the default branch HEAD, id defaults to the
    // repository's own name, and the recorded version is the manifest's own
    // (no ref was explicitly checked out).
    let report = pkg::install_from_git(&config, &ctx, &url, None, None, true).unwrap();
    assert_eq!(report.id, "demo");
    assert_eq!(report.version, "1.0.0");
    let meta = store.get("demo").unwrap().expect("meta.json must exist");
    assert_eq!(meta.source.as_deref(), Some(format!("git:{url}").as_str()));
    assert_eq!(meta.package, None, "no registry entry for a direct install");
    let app_dir = store.app_dir("demo").unwrap();
    assert!(app_dir.join("repository/asc.yaml").exists());
    assert!(app_dir.join("config").is_dir());
    assert!(app_dir.join("data").is_dir());

    // --branch: a second instance tracking `dev` — content from that branch,
    // the branch name itself recorded as the version, custom name honored.
    let report = pkg::install_from_git(
        &config,
        &ctx,
        &url,
        Some(GitRef::Branch("dev")),
        Some("demo-dev"),
        true,
    )
    .unwrap();
    assert_eq!(report.id, "demo-2", "a second instance gets the -2 suffix");
    assert_eq!(report.version, "dev");
    let meta = store.get("demo-2").unwrap().unwrap();
    assert_eq!(meta.custom_name.as_deref(), Some("demo-dev"));
    let manifest =
        fs::read_to_string(store.app_dir("demo-2").unwrap().join("repository/asc.yaml")).unwrap();
    assert!(manifest.contains("1.1.0-dev"), "got: {manifest}");

    // --tag: pins the exact tag, recorded as the version.
    let report =
        pkg::install_from_git(&config, &ctx, &url, Some(GitRef::Tag("v1.0.0")), None, true)
            .unwrap();
    assert_eq!(report.id, "demo-3");
    assert_eq!(report.version, "v1.0.0");

    // An unknown branch fails cleanly, no half-created app directory left.
    let err = pkg::install_from_git(
        &config,
        &ctx,
        &url,
        Some(GitRef::Branch("ghost")),
        None,
        true,
    )
    .unwrap_err();
    assert!(!err.to_string().is_empty());
    assert!(!store.app_dir("demo-4").unwrap().exists());
}

#[test]
fn install_direct_from_git_requires_license_acceptance() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: git is not available");
        return;
    }
    let ws = tempfile::tempdir().unwrap();
    let repo = ws.path().join("licensed");
    fs::create_dir_all(&repo).unwrap();
    fs::write(
        repo.join("asc.yaml"),
        "name: licensed\nversion: 1.0.0\ntype: native\nruntime:\n  start: ./run.sh\n",
    )
    .unwrap();
    fs::write(repo.join("LICENSE.md"), "MIT License\n\nDemo terms.\n").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let url = repo.display().to_string().replace('\\', "/");

    let mut config = Config::default();
    config.daemon.data_dir = ws.path().join("data");
    config.daemon.apps_dir = ws.path().join("apps");
    let ctx = UserContext {
        uid: 1000,
        name: "tester".into(),
        is_root: false,
    };

    let err = pkg::install_from_git(&config, &ctx, &url, None, None, false).unwrap_err();
    let required = err
        .downcast_ref::<pkg::LicenseRequired>()
        .expect("expected the typed license error");
    assert_eq!(required.source, "git");
    assert_eq!(required.git, url);
    assert!(required.license.contains("MIT License"));
    let store = AppStore::new(config.daemon.apps_dir.clone());
    assert!(!store.app_dir("licensed").unwrap().exists());

    // Accepted: installs normally.
    pkg::install_from_git(&config, &ctx, &url, None, None, true).unwrap();
    assert!(store.get("licensed").unwrap().is_some());
}
