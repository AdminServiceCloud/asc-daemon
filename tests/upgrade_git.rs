//! Upgrading an app that was installed straight from a repository URL
//! (DMN-053): there is no registry entry to resolve, so the upgrade has to
//! come back to the URL recorded in `meta.source` — and follow the branch the
//! app tracks when it tracks one, instead of jumping to a tag.

use std::fs;
use std::path::Path;
use std::process::Command;

use asc_daemon::daemon::apps::{AppStore, UserContext};
use asc_daemon::daemon::config::Config;
use asc_daemon::daemon::pkg::{self, GitRef, UpgradeOutcome};

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

fn manifest(version: &str) -> String {
    format!("name: demo\nversion: {version}\ntype: native\nruntime:\n  start: ./run.sh\n")
}

/// A repository with `asc.yaml` at the root, one commit and the tag `v1.0.0`.
fn seed_repo(repo: &Path) {
    fs::create_dir_all(repo).unwrap();
    fs::write(repo.join("asc.yaml"), manifest("1.0.0")).unwrap();
    git(repo, &["init", "-q"]);
    git(repo, &["add", "."]);
    git(repo, &["commit", "-q", "-m", "init"]);
    git(repo, &["tag", "v1.0.0"]);
}

fn workspace(ws: &Path) -> (Config, UserContext, AppStore) {
    let mut config = Config::default();
    config.daemon.data_dir = ws.join("data");
    config.daemon.apps_dir = ws.join("apps");
    let ctx = UserContext {
        uid: 1000,
        name: "tester".into(),
        is_root: false,
    };
    let store = AppStore::new(config.daemon.apps_dir.clone());
    (config, ctx, store)
}

/// A tag-installed app moves to the repository's newest tag — the registry is
/// never consulted, and the app's own URL is the only source involved.
#[test]
fn upgrade_follows_the_recorded_repository_url() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: git is not available");
        return;
    }
    let ws = tempfile::tempdir().unwrap();
    let repo = ws.path().join("demo");
    seed_repo(&repo);
    let url = repo.display().to_string().replace('\\', "/");
    let (config, ctx, store) = workspace(ws.path());

    pkg::install_from_git(
        &config,
        &ctx,
        &url,
        Some(GitRef::Tag("v1.0.0")),
        None,
        true,
        None,
    )
    .unwrap();
    let meta = store.get("demo").unwrap().unwrap();
    assert_eq!(meta.version.as_deref(), Some("v1.0.0"));
    assert_eq!(meta.branch, None, "a tag install tracks no branch");

    // Nothing newer in the repository yet.
    match pkg::upgrade(&config, &ctx, "demo").unwrap() {
        UpgradeOutcome::UpToDate { id, version } => {
            assert_eq!(id, "demo");
            assert_eq!(version, "v1.0.0");
        }
        other => panic!("expected up-to-date, got: {other:?}"),
    }

    // A new release: the upgrade finds it through meta.source alone.
    fs::write(repo.join("asc.yaml"), manifest("1.1.0")).unwrap();
    git(&repo, &["commit", "-q", "-am", "1.1.0"]);
    git(&repo, &["tag", "v1.1.0"]);
    match pkg::upgrade(&config, &ctx, "demo").unwrap() {
        UpgradeOutcome::Upgraded { id, from, to } => {
            assert_eq!(id, "demo");
            assert_eq!(from.as_deref(), Some("v1.0.0"));
            assert_eq!(to, "v1.1.0");
        }
        other => panic!("expected an upgrade, got: {other:?}"),
    }
    let meta = store.get("demo").unwrap().unwrap();
    assert_eq!(meta.version.as_deref(), Some("v1.1.0"));
    assert_eq!(
        meta.source.as_deref(),
        Some(format!("git:{url}").as_str()),
        "the origin survives the upgrade"
    );
    let installed =
        fs::read_to_string(store.app_dir("demo").unwrap().join("repository/asc.yaml")).unwrap();
    assert!(installed.contains("1.1.0"), "got: {installed}");
    // No leftovers of the swap.
    assert!(
        !store
            .app_dir("demo")
            .unwrap()
            .join("repository.new")
            .exists()
    );
    assert!(
        !store
            .app_dir("demo")
            .unwrap()
            .join("repository.old")
            .exists()
    );
}

/// An app installed with `--branch` keeps following that branch: an upgrade
/// re-clones it (even though the repository has newer tags), and reports
/// "up to date" while the branch has not moved.
#[test]
fn branch_installs_follow_their_branch() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: git is not available");
        return;
    }
    let ws = tempfile::tempdir().unwrap();
    let repo = ws.path().join("demo");
    seed_repo(&repo);
    git(&repo, &["checkout", "-q", "-b", "dev"]);
    fs::write(repo.join("asc.yaml"), manifest("1.1.0-dev")).unwrap();
    git(&repo, &["commit", "-q", "-am", "dev"]);
    let url = repo.display().to_string().replace('\\', "/");
    let (config, ctx, store) = workspace(ws.path());

    pkg::install_from_git(
        &config,
        &ctx,
        &url,
        Some(GitRef::Branch("dev")),
        None,
        true,
        None,
    )
    .unwrap();
    let meta = store.get("demo").unwrap().unwrap();
    assert_eq!(meta.branch.as_deref(), Some("dev"));
    assert_eq!(meta.version.as_deref(), Some("dev"));

    // The branch has not moved — and the v1.0.0 tag must not pull the app off
    // the branch it was installed to track.
    match pkg::upgrade(&config, &ctx, "demo").unwrap() {
        UpgradeOutcome::UpToDate { version, .. } => assert_eq!(version, "dev"),
        other => panic!("expected up-to-date, got: {other:?}"),
    }

    // A new commit on the branch is what an upgrade picks up.
    fs::write(repo.join("asc.yaml"), manifest("1.2.0-dev")).unwrap();
    git(&repo, &["commit", "-q", "-am", "more dev"]);
    match pkg::upgrade(&config, &ctx, "demo").unwrap() {
        UpgradeOutcome::Upgraded { from, to, .. } => {
            assert_eq!(from.as_deref(), Some("dev"));
            assert_eq!(to, "dev", "still on the branch, not on a tag");
        }
        other => panic!("expected an upgrade, got: {other:?}"),
    }
    let installed =
        fs::read_to_string(store.app_dir("demo").unwrap().join("repository/asc.yaml")).unwrap();
    assert!(installed.contains("1.2.0-dev"), "got: {installed}");
    assert_eq!(
        store.get("demo").unwrap().unwrap().branch.as_deref(),
        Some("dev"),
        "the tracked branch survives the upgrade"
    );

    // An explicit @version is the user pinning the app to a tag instead.
    match pkg::upgrade(&config, &ctx, "demo@v1.0.0").unwrap() {
        UpgradeOutcome::Upgraded { to, .. } => assert_eq!(to, "v1.0.0"),
        other => panic!("expected an upgrade, got: {other:?}"),
    }
}

/// An untagged repository has no version to move to, so the default branch is
/// the moving ref: the upgrade re-clones its HEAD and takes the version from
/// the new manifest, exactly as the install did.
#[test]
fn untagged_repositories_track_their_default_branch() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: git is not available");
        return;
    }
    let ws = tempfile::tempdir().unwrap();
    let repo = ws.path().join("demo");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("asc.yaml"), manifest("0.1.0")).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let url = repo.display().to_string().replace('\\', "/");
    let (config, ctx, store) = workspace(ws.path());

    pkg::install_from_git(&config, &ctx, &url, None, None, true, None).unwrap();
    assert_eq!(
        store.get("demo").unwrap().unwrap().version.as_deref(),
        Some("0.1.0"),
        "the manifest version, no ref was checked out"
    );

    match pkg::upgrade(&config, &ctx, "demo").unwrap() {
        UpgradeOutcome::UpToDate { version, .. } => assert_eq!(version, "0.1.0"),
        other => panic!("expected up-to-date, got: {other:?}"),
    }

    fs::write(repo.join("asc.yaml"), manifest("0.2.0")).unwrap();
    git(&repo, &["commit", "-q", "-am", "0.2.0"]);
    match pkg::upgrade(&config, &ctx, "demo").unwrap() {
        UpgradeOutcome::Upgraded { from, to, .. } => {
            assert_eq!(from.as_deref(), Some("0.1.0"));
            assert_eq!(to, "0.2.0", "the new manifest's version");
        }
        other => panic!("expected an upgrade, got: {other:?}"),
    }
    assert_eq!(
        store.get("demo").unwrap().unwrap().version.as_deref(),
        Some("0.2.0")
    );
}
