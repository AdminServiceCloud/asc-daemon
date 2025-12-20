//! End-to-end package install: a local git repo as the package source and a
//! `file://` registry. Runs as a separate test binary so the `ASC_SOURCES`
//! environment variable does not race with other tests.

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
fn install_from_file_registry() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: git is not available");
        return;
    }
    let ws = tempfile::tempdir().unwrap();

    // 1. Package repository with asc.yaml, committed and tagged v1.0.0.
    let repo = ws.path().join("demo-repo");
    fs::create_dir_all(&repo).unwrap();
    fs::write(
        repo.join("asc.yaml"),
        "name: demo\nversion: 1.0.0\ntype: native\nruntime:\n  start: ./run.sh\n",
    )
    .unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    git(&repo, &["tag", "v1.0.0"]);

    // 2. file:// registry pointing at that repository.
    let reg = ws.path().join("registry");
    fs::create_dir_all(reg.join("categories")).unwrap();
    let repo_url = repo.display().to_string().replace('\\', "/");
    fs::write(
        reg.join("registry.json"),
        r#"{"name":"local","categories":[{"name":"web","index":"categories/web.json"}]}"#,
    )
    .unwrap();
    fs::write(
        reg.join("categories/web.json"),
        format!(
            r#"{{"category":"web","packages":[{{"name":"demo","type":"app","latest":"1.0.0","description":"Demo","source":{{"git":"{repo_url}"}}}}]}}"#
        ),
    )
    .unwrap();

    // 3. Sources file with only this local registry.
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

    let mut config = Config::default();
    config.daemon.data_dir = ws.path().join("data");
    config.daemon.apps_dir = ws.path().join("apps");
    let ctx = UserContext {
        uid: 1000,
        name: "tester".into(),
        is_root: false,
    };

    // Root policy "docker": a regular user cannot install a native package,
    // and the failed install leaves no half-created app directory behind.
    {
        let mut restricted = Config::default();
        restricted.daemon.data_dir = config.daemon.data_dir.clone();
        restricted.daemon.apps_dir = config.daemon.apps_dir.clone();
        restricted.policy.user_install = asc_daemon::daemon::config::UserInstall::Docker;
        let err = pkg::install(&restricted, &ctx, "demo@1.0.0").unwrap_err();
        assert!(err.to_string().contains("Docker"), "got: {err:#}");
        let store = AppStore::new(restricted.daemon.apps_dir.clone());
        assert!(!store.app_dir("demo").unwrap().exists());

        // The same policy does not restrict root.
        let root_ctx = UserContext {
            uid: 0,
            name: "root".into(),
            is_root: true,
        };
        pkg::install(&restricted, &root_ctx, "demo@1.0.0").unwrap();
        store.remove("demo").unwrap();
    }

    // Requested tag is `1.0.0`, the repo has `v1.0.0` — the fallback must hit.
    let report = pkg::install(&config, &ctx, "demo@1.0.0").unwrap();
    assert_eq!(report.id, "demo");
    assert_eq!(report.version, "v1.0.0");

    let store = AppStore::new(config.daemon.apps_dir.clone());
    let meta = store.get("demo").unwrap().expect("meta.json must exist");
    assert_eq!(meta.owner.uid, 1000);
    assert_eq!(meta.version.as_deref(), Some("v1.0.0"));
    assert!(meta.source.as_deref().unwrap().starts_with("local:"));
    let app_dir = store.app_dir("demo").unwrap();
    assert!(app_dir.join("repository/asc.yaml").exists());
    assert!(app_dir.join("config").is_dir());
    assert!(app_dir.join("data").is_dir());

    // Installing on top of an existing app must fail cleanly.
    let err = pkg::install(&config, &ctx, "demo").unwrap_err();
    assert!(err.to_string().contains("demo"));

    // Unknown packages fail with the "not found" error, not a panic/partial state.
    assert!(pkg::install(&config, &ctx, "ghost").is_err());
    assert!(!store.app_dir("ghost").unwrap().exists());
}
