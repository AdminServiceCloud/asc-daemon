//! Direct git install of a stack (`asc install <url> --path <dir>`, DMN-097):
//! the repository ships `asc.stack.yaml` instead of `asc.yaml` and there is no
//! registry entry to say so beforehand — the clone decides. Runs as its own
//! test binary; no registry sources are involved at all.

use std::fs;
use std::path::Path;
use std::process::Command;

use asc_daemon::daemon::apps::{AppStore, UserContext};
use asc_daemon::daemon::config::Config;
use asc_daemon::daemon::pkg;

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

fn stack(outcome: pkg::InstallOutcome) -> (Vec<String>, Vec<String>) {
    match outcome {
        pkg::InstallOutcome::Stack {
            installed, skipped, ..
        } => (
            installed.into_iter().map(|r| r.id).collect(),
            skipped.into_iter().collect(),
        ),
        other => panic!("expected a stack install, got {other:?}"),
    }
}

#[test]
fn install_stack_directly_from_a_monorepo_path() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: git is not available");
        return;
    }
    let ws = tempfile::tempdir().unwrap();

    // A monorepo whose stack lives in gameservers/demo — the shape of
    // asc-example-apps, which is what the platform's install dialog clones.
    let repo = ws.path().join("monorepo");
    let stack_dir = repo.join("gameservers/demo");
    for (dir, name) in [
        ("master", "demo-master"),
        ("server", "demo-server"),
        ("extras", "demo-extras"),
    ] {
        fs::create_dir_all(stack_dir.join(dir)).unwrap();
        fs::write(
            stack_dir.join(dir).join("asc.yaml"),
            format!("name: {name}\nversion: 1.0.0\ntype: native\nruntime:\n  start: ./run.sh\n"),
        )
        .unwrap();
    }
    fs::write(
        stack_dir.join("asc.stack.yaml"),
        r#"
name: demo
version: 1.0.0
apps:
  - { name: master, path: ./master }
  - { name: server, path: ./server, depends_on: [master] }
  - { name: extras, path: ./extras, optional: true }
"#,
    )
    .unwrap();
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
    let store = AppStore::new(config.daemon.apps_dir.clone());

    // No registry entry, no `--app`: the install starts out as a single-app
    // one, finds asc.stack.yaml where asc.yaml should have been and installs
    // every non-optional app of the stack instead, dependencies first.
    let (installed, skipped) = stack(
        pkg::install_from_git(
            &config,
            &ctx,
            &url,
            None,
            Some("gameservers/demo"),
            None,
            None,
            true,
            None,
            false,
            None,
            None,
        )
        .unwrap(),
    );
    assert_eq!(installed, ["demo-master", "demo-server"]);
    assert!(skipped.is_empty(), "optional apps are not installed");
    assert!(
        !store.app_dir("demo").unwrap().exists(),
        "the probe directory named after the path is cleaned up"
    );

    // Each app records the repository and its own manifest subdirectory — a
    // stack app installed this way upgrades from the app's asc.yaml, not from
    // the stack root.
    let meta = store.get("demo-server").unwrap().expect("meta.json");
    assert_eq!(meta.source.as_deref(), Some(format!("git:{url}").as_str()));
    assert_eq!(meta.repo_path.as_deref(), Some("gameservers/demo/server"));
    assert_eq!(meta.package, None, "no registry entry for a direct install");
    assert!(
        store
            .app_dir("demo-server")
            .unwrap()
            .join("repository/gameservers/demo/server/asc.yaml")
            .exists()
    );

    // `--app` installs one app of the stack — including an optional one,
    // which a whole-stack install skips — and reuses its already-installed
    // dependencies instead of duplicating them.
    let (installed, skipped) = stack(
        pkg::install_from_git(
            &config,
            &ctx,
            &url,
            None,
            Some("gameservers/demo"),
            Some("extras"),
            None,
            true,
            None,
            false,
            None,
            None,
        )
        .unwrap(),
    );
    assert_eq!(installed, ["demo-extras"]);
    assert!(skipped.is_empty());

    let (installed, skipped) = stack(
        pkg::install_from_git(
            &config,
            &ctx,
            &url,
            None,
            Some("gameservers/demo"),
            Some("server"),
            None,
            true,
            None,
            false,
            None,
            None,
        )
        .unwrap(),
    );
    assert_eq!(installed, ["demo-server-2"]);
    assert_eq!(skipped, ["demo-master"]);

    // An app the stack does not ship fails cleanly.
    let err = pkg::install_from_git(
        &config,
        &ctx,
        &url,
        None,
        Some("gameservers/demo"),
        Some("ghost"),
        None,
        true,
        None,
        false,
        None,
        None,
    )
    .unwrap_err();
    assert!(err.to_string().contains("ghost"), "got: {err:#}");

    // The recorded subdirectory is what an upgrade reads the app manifest
    // from: bump the app's own manifest and upgrade it by tag.
    fs::write(
        stack_dir.join("server/asc.yaml"),
        "name: demo-server\nversion: 2.0.0\ntype: native\nruntime:\n  start: ./run.sh\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "v2"]);
    git(&repo, &["tag", "v2.0.0"]);

    match pkg::upgrade(&config, &ctx, "demo-server@2.0.0", None).unwrap() {
        pkg::UpgradeOutcome::Upgraded { id, to, .. } => {
            assert_eq!(id, "demo-server");
            assert_eq!(to, "v2.0.0");
        }
        other => panic!("expected an upgrade, got {other:?}"),
    }
}

#[test]
fn install_stack_directly_from_a_repository_root() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: git is not available");
        return;
    }
    let ws = tempfile::tempdir().unwrap();

    let repo = ws.path().join("rootstack");
    for (dir, name) in [("one", "root-one"), ("two", "root-two")] {
        fs::create_dir_all(repo.join(dir)).unwrap();
        fs::write(
            repo.join(dir).join("asc.yaml"),
            format!("name: {name}\nversion: 1.0.0\ntype: native\nruntime:\n  start: ./run.sh\n"),
        )
        .unwrap();
    }
    fs::write(
        repo.join("asc.stack.yaml"),
        "name: rootstack\nversion: 1.0.0\napps:\n  - { name: one, path: ./one }\n  - { name: two, path: ./two }\n",
    )
    .unwrap();
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

    // The same detection without a `--path`: a stack at the repository root.
    let (installed, _) = stack(
        pkg::install_from_git(
            &config, &ctx, &url, None, None, None, None, true, None, false, None, None,
        )
        .unwrap(),
    );
    assert_eq!(installed, ["root-one", "root-two"]);

    let store = AppStore::new(config.daemon.apps_dir.clone());
    assert_eq!(
        store.get("root-one").unwrap().unwrap().repo_path.as_deref(),
        Some("one"),
        "the app's own directory inside the repository"
    );
}
