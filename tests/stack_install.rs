//! End-to-end stack install (`asc.stack.yaml`): a local git repo holding two
//! apps + a `file://` registry with a `stack` package. Runs as a separate
//! test binary so the `ASC_SOURCES` environment variable does not race with
//! other tests.

use std::fs;
use std::path::Path;
use std::process::Command;

use asc_daemon::daemon::apps::{AppStore, UserContext};
use asc_daemon::daemon::config::Config;
use asc_daemon::daemon::pkg::{self, registry::file_source_url};

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
fn install_stack_from_file_registry() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: git is not available");
        return;
    }
    let ws = tempfile::tempdir().unwrap();

    // 1. Stack repository: asc.stack.yaml + master/ + server/ + extras/ (optional).
    let repo = ws.path().join("stack-repo");
    for (dir, name) in [
        ("master", "demo-master"),
        ("server", "demo-server"),
        ("extras", "demo-extras"),
    ] {
        fs::create_dir_all(repo.join(dir)).unwrap();
        fs::write(
            repo.join(dir).join("asc.yaml"),
            format!("name: {name}\nversion: 1.0.0\ntype: native\nruntime:\n  start: ./run.sh\n"),
        )
        .unwrap();
    }
    fs::write(
        repo.join("asc.stack.yaml"),
        r#"
name: demo-stack
version: 1.0.0
apps:
  - { name: master, path: ./master }
  - { name: server, path: ./server, depends_on: [master] }
  - { name: extras, path: ./extras, optional: true }
env:
  - name: SHARED
    default: from-stack
"#,
    )
    .unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    git(&repo, &["tag", "v1.0.0"]);

    // 2. file:// registry with a `stack` package entry.
    let reg = ws.path().join("registry");
    fs::create_dir_all(reg.join("categories")).unwrap();
    let repo_url = repo.display().to_string().replace('\\', "/");
    fs::write(
        reg.join("registry.json"),
        r#"{"name":"local","categories":[{"name":"game-servers","index":"categories/game-servers.json"}]}"#,
    )
    .unwrap();
    fs::write(
        reg.join("categories/game-servers.json"),
        format!(
            r#"{{"category":"game-servers","packages":[{{"name":"demo-stack","type":"stack","latest":"1.0.0","description":"Demo stack","source":{{"git":"{repo_url}"}}}}]}}"#
        ),
    )
    .unwrap();

    let sources_path = ws.path().join("sources.toml");
    fs::write(
        &sources_path,
        format!(
            "[[source]]\nname = \"local\"\nurl = \"{}\"\n",
            file_source_url(&reg)
        ),
    )
    .unwrap();
    // Safe: this is the only test in this binary touching the environment.
    unsafe { std::env::set_var("ASC_SOURCES", &sources_path) };
    unsafe { std::env::set_var("ASC_USER_SOURCES", ws.path().join("user-sources.toml")) };
    unsafe { std::env::set_var("XDG_CACHE_HOME", ws.path().join("cache")) };

    let mut config = Config::default();
    config.daemon.data_dir = ws.path().join("data");
    config.daemon.apps_dir = ws.path().join("apps");
    let ctx = UserContext {
        uid: 1000,
        name: "tester".into(),
        is_root: false,
    };
    let store = AppStore::new(config.daemon.apps_dir.clone());

    // ── Whole stack: dependency order, optional apps skipped ─────────────
    let pkg::InstallOutcome::Stack {
        stack,
        installed,
        skipped,
    } = pkg::install(&config, &ctx, "demo-stack", None).unwrap()
    else {
        panic!("expected a stack install");
    };
    assert_eq!(stack, "demo-stack");
    let ids: Vec<&str> = installed.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(
        ids,
        ["demo-master", "demo-server"],
        "deps first, no optional"
    );
    assert!(skipped.is_empty());

    // App ids come from each app's own asc.yaml; meta records the stack spec.
    let meta = store.get("demo-server").unwrap().expect("meta.json");
    assert_eq!(meta.package.as_deref(), Some("demo-stack/server"));
    assert_eq!(meta.version.as_deref(), Some("v1.0.0"));
    let repo_yaml = store
        .app_dir("demo-server")
        .unwrap()
        .join("repository/server/asc.yaml");
    assert!(repo_yaml.exists(), "every app owns a full repository clone");

    // Stack env is merged into the app env (docker apps would receive it;
    // for native apps it is at least recorded in the cloned manifest dir).
    // The master app has no stack spec suffix mixup:
    assert_eq!(
        store
            .get("demo-master")
            .unwrap()
            .unwrap()
            .package
            .as_deref(),
        Some("demo-stack/master")
    );

    // ── Re-install: everything is skipped, nothing breaks ────────────────
    let pkg::InstallOutcome::Stack {
        installed, skipped, ..
    } = pkg::install(&config, &ctx, "demo-stack", None).unwrap()
    else {
        panic!("expected a stack install");
    };
    assert!(installed.is_empty());
    assert_eq!(skipped, ["demo-master", "demo-server"]);

    // ── Single app from the stack (optional installs when asked) ─────────
    let pkg::InstallOutcome::Stack {
        installed, skipped, ..
    } = pkg::install(&config, &ctx, "demo-stack/extras", None).unwrap()
    else {
        panic!("expected a stack install");
    };
    assert_eq!(installed.len(), 1);
    assert_eq!(installed[0].id, "demo-extras");
    assert!(skipped.is_empty());

    // Unknown stack app fails cleanly.
    let err = pkg::install(&config, &ctx, "demo-stack/ghost", None).unwrap_err();
    assert!(err.to_string().contains("ghost"), "got: {err:#}");

    // ── Upgrade one app of the stack to a new tag ─────────────────────────
    fs::write(
        repo.join("server/asc.yaml"),
        "name: demo-server\nversion: 2.0.0\ntype: native\nruntime:\n  start: ./run.sh\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "v2"]);
    git(&repo, &["tag", "v2.0.0"]);

    match pkg::upgrade(&config, &ctx, "demo-server@2.0.0").unwrap() {
        pkg::UpgradeOutcome::Upgraded { id, from, to } => {
            assert_eq!(id, "demo-server");
            assert_eq!(from.as_deref(), Some("v1.0.0"));
            assert_eq!(to, "v2.0.0");
        }
        other => panic!("expected an upgrade, got {other:?}"),
    }
    let meta = store.get("demo-server").unwrap().unwrap();
    assert_eq!(meta.version.as_deref(), Some("v2.0.0"));
    assert_eq!(
        meta.package.as_deref(),
        Some("demo-stack/server"),
        "stack origin survives the upgrade"
    );
}
