//! Package name conflicts between sources: without an explicit source the
//! install fails with a typed error listing every candidate; `--source`
//! pins the registry; upgrades prefer the source the app came from. Runs as
//! a separate test binary so `ASC_SOURCES` does not race with other tests.

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

/// One package repository tagged v1.0.0 (and v2.0.0 for upgrades).
fn make_repo(dir: &Path, version_two: bool) {
    fs::create_dir_all(dir).unwrap();
    fs::write(
        dir.join("asc.yaml"),
        "name: demo\nversion: 1.0.0\ntype: native\nruntime:\n  start: ./run.sh\n",
    )
    .unwrap();
    git(dir, &["init", "-q"]);
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
    git(dir, &["tag", "v1.0.0"]);
    if version_two {
        fs::write(
            dir.join("asc.yaml"),
            "name: demo\nversion: 2.0.0\ntype: native\nruntime:\n  start: ./run.sh\n",
        )
        .unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-q", "-m", "v2"]);
        git(dir, &["tag", "v2.0.0"]);
    }
}

/// A file:// registry with a single `demo` package pointing at `repo`.
fn make_registry(dir: &Path, repo: &Path) {
    fs::create_dir_all(dir.join("categories")).unwrap();
    let repo_url = repo.display().to_string().replace('\\', "/");
    fs::write(
        dir.join("registry.json"),
        r#"{"name":"local","categories":[{"name":"web","index":"categories/web.json"}]}"#,
    )
    .unwrap();
    fs::write(
        dir.join("categories/web.json"),
        format!(
            r#"{{"category":"web","packages":[{{"name":"demo","type":"app","latest":"1.0.0","description":"Demo","source":{{"git":"{repo_url}"}}}}]}}"#
        ),
    )
    .unwrap();
}

#[test]
fn ambiguous_package_requires_source_choice() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: git is not available");
        return;
    }
    let ws = tempfile::tempdir().unwrap();

    // Two registries both provide `demo`; `beta`'s repo also has v2.0.0.
    let repo_alpha = ws.path().join("repo-alpha");
    let repo_beta = ws.path().join("repo-beta");
    make_repo(&repo_alpha, false);
    make_repo(&repo_beta, true);
    let reg_alpha = ws.path().join("reg-alpha");
    let reg_beta = ws.path().join("reg-beta");
    make_registry(&reg_alpha, &repo_alpha);
    make_registry(&reg_beta, &repo_beta);

    let sources_path = ws.path().join("sources.toml");
    fs::write(
        &sources_path,
        format!(
            "[[source]]\nname = \"alpha\"\nurl = \"{}\"\n\n[[source]]\nname = \"beta\"\nurl = \"{}\"\n",
            file_source_url(&reg_alpha),
            file_source_url(&reg_beta)
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

    // No explicit source → typed error listing both candidates in priority order.
    let err = pkg::install(&config, &ctx, "demo", None).unwrap_err();
    let ambiguous = err
        .downcast_ref::<pkg::AmbiguousPackage>()
        .expect("expected AmbiguousPackage");
    assert_eq!(ambiguous.name, "demo");
    let sources: Vec<&str> = ambiguous
        .candidates
        .iter()
        .map(|(s, _)| s.as_str())
        .collect();
    assert_eq!(sources, ["alpha", "beta"]);
    assert!(
        ambiguous.candidates.iter().all(|(_, git)| !git.is_empty()),
        "candidates carry the repository URL"
    );
    assert!(!store.app_dir("demo").unwrap().exists(), "nothing installed");

    // Unknown source name fails cleanly.
    let err = pkg::install(&config, &ctx, "demo", Some("ghost")).unwrap_err();
    assert!(err.to_string().contains("ghost"), "got: {err:#}");

    // Explicit source pins the registry.
    let pkg::InstallOutcome::App(report) =
        pkg::install(&config, &ctx, "demo", Some("beta")).unwrap()
    else {
        panic!("expected a single-app install");
    };
    assert_eq!(report.id, "demo");
    let meta = store.get("demo").unwrap().expect("meta.json");
    assert!(meta.source.as_deref().unwrap().starts_with("beta:"));

    // Upgrade resolves through the stored source (beta has v2.0.0, alpha
    // does not) — no ambiguity error, no accidental switch to alpha.
    match pkg::upgrade(&config, &ctx, "demo@2.0.0").unwrap() {
        pkg::UpgradeOutcome::Upgraded { to, .. } => assert_eq!(to, "v2.0.0"),
        other => panic!("expected an upgrade, got {other:?}"),
    }
    assert!(
        store
            .get("demo")
            .unwrap()
            .unwrap()
            .source
            .as_deref()
            .unwrap()
            .starts_with("beta:")
    );

    // A single provider never asks: remove the conflict and install again.
    store.remove("demo").unwrap();
    fs::write(
        reg_alpha.join("categories/web.json"),
        r#"{"category":"web","packages":[]}"#,
    )
    .unwrap();
    // Bypass the index cache so the edit is visible immediately.
    let _ = fs::remove_dir_all(ws.path().join("cache"));
    assert!(pkg::install(&config, &ctx, "demo", None).is_ok());
}
