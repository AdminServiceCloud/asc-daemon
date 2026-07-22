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
    fs::write(repo.join("LICENSE.md"), "MIT License\n\nDemo terms.\n").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    git(&repo, &["tag", "v1.0.0"]);

    // 1b. Monorepo whose package lives in a subdirectory and is licensed
    // there only — the clone root has no LICENSE file.
    let mono = ws.path().join("mono-repo");
    fs::create_dir_all(mono.join("pkg")).unwrap();
    fs::write(
        mono.join("pkg/asc.yaml"),
        "name: subdemo\nversion: 1.0.0\ntype: native\nruntime:\n  start: ./run.sh\n",
    )
    .unwrap();
    fs::write(mono.join("pkg/LICENSE"), "Sub terms.\n").unwrap();
    fs::write(mono.join("README.md"), "no root license\n").unwrap();
    git(&mono, &["init", "-q"]);
    git(&mono, &["add", "."]);
    git(&mono, &["commit", "-q", "-m", "init"]);
    git(&mono, &["tag", "v1.0.0"]);

    // 2. file:// registry pointing at those repositories.
    let reg = ws.path().join("registry");
    fs::create_dir_all(reg.join("categories")).unwrap();
    let repo_url = repo.display().to_string().replace('\\', "/");
    let mono_url = mono.display().to_string().replace('\\', "/");
    fs::write(
        reg.join("registry.json"),
        r#"{"name":"local","categories":[{"name":"web","index":"categories/web.json"}]}"#,
    )
    .unwrap();
    fs::write(
        reg.join("categories/web.json"),
        format!(
            r#"{{"category":"web","packages":[{{"name":"demo","type":"app","description":"Demo","source":{{"git":"{repo_url}"}}}},{{"name":"subdemo","type":"app","description":"Sub","source":{{"git":"{mono_url}","path":"pkg"}}}}]}}"#
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
    // The user list and the index cache are pointed into the tempdir too,
    // keeping the test hermetic (no reads of ~/.config, no writes to ~/.cache).
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

    // Root policy "docker": a regular user cannot install a native package,
    // and the failed install leaves no half-created app directory behind.
    {
        let mut restricted = Config::default();
        restricted.daemon.data_dir = config.daemon.data_dir.clone();
        restricted.daemon.apps_dir = config.daemon.apps_dir.clone();
        restricted.policy.user_install = asc_daemon::daemon::config::UserInstall::Docker;
        let err =
            pkg::install(&restricted, &ctx, "demo@1.0.0", None, None, true, None).unwrap_err();
        assert!(err.to_string().contains("Docker"), "got: {err:#}");
        let store = AppStore::new(restricted.daemon.apps_dir.clone());
        assert!(!store.app_dir("demo").unwrap().exists());

        // The same policy does not restrict root.
        let root_ctx = UserContext {
            uid: 0,
            name: "root".into(),
            is_root: true,
        };
        pkg::install(&restricted, &root_ctx, "demo@1.0.0", None, None, true, None).unwrap();
        store.remove("demo").unwrap();
    }

    // The repository ships LICENSE.md: installing without acceptance raises
    // the typed error (source + repository + text), leaving nothing behind.
    {
        let err = pkg::install(&config, &ctx, "demo@1.0.0", None, None, false, None).unwrap_err();
        let required = err
            .downcast_ref::<pkg::LicenseRequired>()
            .expect("expected the typed license error");
        assert_eq!(required.source, "local");
        assert_eq!(required.package, "demo");
        assert!(required.license.contains("MIT License"));
        let store = AppStore::new(config.daemon.apps_dir.clone());
        assert!(!store.app_dir("demo").unwrap().exists());
    }

    // A monorepo package licensed only in its own subdirectory (no LICENSE
    // at the clone root) still requires acceptance.
    {
        let err =
            pkg::install(&config, &ctx, "subdemo@1.0.0", None, None, false, None).unwrap_err();
        let required = err
            .downcast_ref::<pkg::LicenseRequired>()
            .expect("expected the typed license error for a subdir license");
        assert!(required.license.contains("Sub terms"));
        let store = AppStore::new(config.daemon.apps_dir.clone());
        assert!(!store.app_dir("subdemo").unwrap().exists());
    }

    // Requested tag is `1.0.0`, the repo has `v1.0.0` — the fallback must hit.
    let pkg::InstallOutcome::App(report) =
        pkg::install(&config, &ctx, "demo@1.0.0", None, None, true, None).unwrap()
    else {
        panic!("expected a single-app install");
    };
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

    // A second install of the same package becomes a new instance (DMN-033):
    // the id gets the first free '-N' suffix, the id doubles as the display
    // name, and the registry package is recorded for upgrades.
    let pkg::InstallOutcome::App(second) =
        pkg::install(&config, &ctx, "demo@1.0.0", None, None, true, None).unwrap()
    else {
        panic!("expected a single-app install");
    };
    assert_eq!(second.id, "demo-2");
    let meta2 = store.get("demo-2").unwrap().expect("meta.json must exist");
    assert_eq!(meta2.custom_name.as_deref(), Some("demo-2"));
    assert_eq!(meta2.package.as_deref(), Some("demo"));
    // An explicit --name wins over the generated one.
    let pkg::InstallOutcome::App(third) = pkg::install(
        &config,
        &ctx,
        "demo@1.0.0",
        None,
        Some("My Demo"),
        true,
        None,
    )
    .unwrap() else {
        panic!("expected a single-app install");
    };
    assert_eq!(third.id, "demo-3");
    let meta3 = store.get("demo-3").unwrap().expect("meta.json must exist");
    assert_eq!(meta3.custom_name.as_deref(), Some("My Demo"));
    store.remove("demo-3").unwrap();

    // Unknown packages fail with the "not found" error, not a panic/partial state.
    assert!(pkg::install(&config, &ctx, "ghost", None, None, true, None).is_err());
    assert!(!store.app_dir("ghost").unwrap().exists());

    // ── Upgrade: a new tag in the package repository ─────────────────────
    fs::write(
        repo.join("asc.yaml"),
        "name: demo\nversion: 2.0.0\ntype: native\nruntime:\n  start: ./run.sh\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "v2"]);
    git(&repo, &["tag", "v2.0.0"]);

    // Explicitly pinned version: no registry index refresh required.
    match pkg::upgrade(&config, &ctx, "demo@2.0.0").unwrap() {
        pkg::UpgradeOutcome::Upgraded { id, from, to } => {
            assert_eq!(id, "demo");
            assert_eq!(from.as_deref(), Some("v1.0.0"));
            assert_eq!(to, "v2.0.0");
        }
        other => panic!("expected an upgrade, got {other:?}"),
    }
    let meta = store.get("demo").unwrap().expect("meta.json must exist");
    assert_eq!(meta.version.as_deref(), Some("v2.0.0"));
    assert_eq!(meta.owner.uid, 1000, "owner survives the upgrade");
    assert!(app_dir.join("repository/asc.yaml").exists());
    assert!(!app_dir.join("repository.new").exists(), "no leftovers");
    assert!(!app_dir.join("repository.old").exists(), "no leftovers");

    // The same version again reports up-to-date instead of recloning.
    match pkg::upgrade(&config, &ctx, "demo@2.0.0").unwrap() {
        pkg::UpgradeOutcome::UpToDate { version, .. } => assert_eq!(version, "v2.0.0"),
        other => panic!("expected up-to-date, got {other:?}"),
    }

    // A suffixed instance resolves upgrades through its recorded package:
    // 'demo-2' is not a registry name, meta.package points it at 'demo'.
    match pkg::upgrade(&config, &ctx, "demo-2@2.0.0").unwrap() {
        pkg::UpgradeOutcome::Upgraded { id, from, to } => {
            assert_eq!(id, "demo-2");
            assert_eq!(from.as_deref(), Some("v1.0.0"));
            assert_eq!(to, "v2.0.0");
        }
        other => panic!("expected an upgrade of the instance, got {other:?}"),
    }
    store.remove("demo-2").unwrap();
    assert!(
        store.get("demo").unwrap().is_some(),
        "removing an instance must not touch the first install"
    );

    // A missing tag fails before touching the installed repository.
    assert!(pkg::upgrade(&config, &ctx, "demo@9.9.9").is_err());
    assert_eq!(
        store.get("demo").unwrap().unwrap().version.as_deref(),
        Some("v2.0.0")
    );
    assert!(app_dir.join("repository/asc.yaml").exists());

    // Upgrading an unknown app fails cleanly.
    assert!(pkg::upgrade(&config, &ctx, "ghost").is_err());

    // ── DMN-047: no @version installs the repository's newest tag ────────
    // The repo now has v1.0.0 and v2.0.0; `demo` (no version) must resolve
    // v2.0.0 from the tags, not any registry field.
    let pkg::InstallOutcome::App(latest) =
        pkg::install(&config, &ctx, "demo", None, None, true, None).unwrap()
    else {
        panic!("expected a single-app install");
    };
    assert_eq!(
        store.get(&latest.id).unwrap().unwrap().version.as_deref(),
        Some("v2.0.0"),
        "no-version install resolves the newest git tag"
    );
    store.remove(&latest.id).unwrap();

    // ── DMN-048: `demo@` asks which version, listing tags and branches ───
    let err = pkg::install(&config, &ctx, "demo@", None, None, true, None).unwrap_err();
    let choice = err
        .downcast_ref::<pkg::VersionChoiceRequired>()
        .unwrap_or_else(|| panic!("expected VersionChoiceRequired, got: {err:#}"));
    assert_eq!(choice.package, "demo");
    // Tags are newest-first; the default branch is offered too.
    assert_eq!(choice.tags, vec!["v2.0.0", "v1.0.0"]);
    assert!(
        choice.branches.iter().any(|b| b == "main" || b == "master"),
        "branches listed: {:?}",
        choice.branches
    );
    // The picker raises before any install work, so no new instance appears
    // (the original `demo` from earlier in the test is untouched).
    assert!(
        !store.app_dir("demo-2").unwrap().exists(),
        "asking for a version installs nothing new"
    );
}
