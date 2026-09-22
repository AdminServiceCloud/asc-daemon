//! End-to-end install from a bare Dockerfile with no `asc.yaml` (DMN-107).
//! Gated behind `ASC_DAEMON_TEST_DOCKER=1` since it needs a live Docker
//! daemon (the Docker runtime always builds/creates a real container,
//! `install_method` or not) — mirrors `tests/exec_docker.rs`.

use std::fs;
use std::path::Path;
use std::process::Command;

use asc_daemon::daemon::apps::meta::InstallMethod as StoredInstallMethod;
use asc_daemon::daemon::apps::{AppStore, UserContext};
use asc_daemon::daemon::config::Config;
use asc_daemon::daemon::pkg::{self, InstallMethod};

fn docker_enabled() -> bool {
    std::env::var("ASC_DAEMON_TEST_DOCKER").as_deref() == Ok("1")
}

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
fn install_from_a_bare_dockerfile_synthesizes_ports_and_volumes() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: git is not available");
        return;
    }
    if !docker_enabled() {
        eprintln!("skipping: set ASC_DAEMON_TEST_DOCKER=1 to run (needs a live Docker daemon)");
        return;
    }
    let ws = tempfile::tempdir().unwrap();

    let repo = ws.path().join("demo");
    fs::create_dir_all(&repo).unwrap();
    fs::write(
        repo.join("Dockerfile"),
        "FROM alpine:3.20\nEXPOSE 8080\nVOLUME /data\nCMD [\"sleep\", \"3600\"]\n",
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
        uid: 0,
        name: "root".into(),
        is_root: true,
    };
    let store = AppStore::new(config.daemon.apps_dir.clone());

    let outcome = pkg::install_from_git(
        &config,
        &ctx,
        &url,
        None,
        None,
        None,
        None,
        true,
        None,
        false,
        Some(InstallMethod::Dockerfile),
        None,
    )
    .unwrap();
    let report = match outcome {
        pkg::InstallOutcome::App(report) => report,
        other => panic!("expected an app install, got {other:?}"),
    };
    assert_eq!(report.id, "demo");

    let meta = store.get("demo").unwrap().expect("meta.json must exist");
    assert_eq!(
        meta.install_method,
        Some(StoredInstallMethod::Dockerfile {
            dockerfile: "Dockerfile".to_string()
        })
    );
    // No tag was checked out — nothing meaningful to show as a version.
    assert_eq!(meta.version, None);
    assert!(matches!(
        meta.runtime,
        asc_daemon::daemon::apps::meta::Runtime::Docker { .. }
    ));

    // EXPOSE 8080 / VOLUME /data were synthesized into settings and merged
    // into config/settings.json's defaults at install time.
    let app_dir = store.app_dir("demo").unwrap();
    let values =
        fs::read_to_string(app_dir.join("config/settings.json")).expect("settings.json exists");
    assert!(values.contains("8080"), "got: {values}");
    assert!(values.contains("/data"), "got: {values}");

    // Disk usage / ports resolution re-synthesize the same manifest+settings
    // rather than failing outright for the missing asc.yaml.
    let usage = asc_daemon::daemon::apps::disk::usage(&config, &store, &meta).unwrap();
    assert!(usage.image_bytes.is_some(), "image was built");
    let ports = asc_daemon::daemon::apps::ports::published(&config, &store, &meta).unwrap();
    assert!(ports.iter().any(|p| p.container == 8080), "got: {ports:?}");
}
