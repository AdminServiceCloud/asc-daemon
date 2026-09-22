//! End-to-end install from a bare `docker-compose.yml` with no `asc.yaml`
//! (DMN-108). Gated behind `ASC_DAEMON_TEST_DOCKER=1` since it needs a live
//! Docker daemon **and** the `docker compose` CLI plugin — mirrors
//! `tests/install_dockerfile.rs`.

use std::fs;
use std::path::Path;
use std::process::Command;

use asc_daemon::daemon::apps::meta::Runtime;
use asc_daemon::daemon::apps::{AppManager, AppStore, RuntimeState, UserContext};
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
fn install_from_a_bare_compose_file_runs_the_project() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: git is not available");
        return;
    }
    if !docker_enabled() {
        eprintln!(
            "skipping: set ASC_DAEMON_TEST_DOCKER=1 to run (needs a live Docker daemon + compose plugin)"
        );
        return;
    }

    let ws = tempfile::tempdir().unwrap();
    let repo = ws.path().join("demo");
    fs::create_dir_all(&repo).unwrap();
    fs::write(
        repo.join("docker-compose.yml"),
        "services:\n  web:\n    image: alpine:3.20\n    command: [\"sh\", \"-c\", \"echo hello-from-web; sleep 3600\"]\n    ports:\n      - \"18080:8080\"\n",
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
    let manager = AppManager::new(&config);

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
        Some(InstallMethod::DockerCompose),
        None,
    )
    .unwrap();
    let report = match outcome {
        pkg::InstallOutcome::App(report) => report,
        other => panic!("expected an app install, got {other:?}"),
    };
    assert_eq!(report.id, "demo");

    let meta = store.get("demo").unwrap().expect("meta.json must exist");
    let Runtime::Compose { project, .. } = &meta.runtime else {
        panic!("expected a compose runtime, got {:?}", meta.runtime);
    };
    assert_eq!(project.as_str(), "asc-demo");
    // Created but not started (DMN-108's provisioning contract, matching a
    // normal Docker app's `docker_create`).
    assert_eq!(
        manager.status(&ctx, "demo").unwrap().state,
        RuntimeState::Stopped
    );

    // Ports come from the compose file itself, live or stopped alike.
    let ports = asc_daemon::daemon::apps::ports::published(&config, &store, &meta).unwrap();
    assert!(
        ports.iter().any(|p| p.host == 18080 && p.container == 8080),
        "got: {ports:?}"
    );

    let cleanup = || {
        let _ = manager.remove(&ctx, "demo");
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        manager.start(&ctx, "demo").unwrap();
        assert_eq!(
            manager.status(&ctx, "demo").unwrap().state,
            RuntimeState::Running
        );

        let logs = manager.logs(&ctx, "demo", 50, false).unwrap();
        // compose logs prefix each line with the service name.
        assert!(logs.contains("web"), "got: {logs}");

        manager.stop(&ctx, "demo").unwrap();
        assert_eq!(
            manager.status(&ctx, "demo").unwrap().state,
            RuntimeState::Stopped
        );
    }));
    cleanup();
    result.unwrap();
}

#[test]
fn a_compose_file_with_a_host_bind_mount_is_refused_at_install() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: git is not available");
        return;
    }
    if !docker_enabled() {
        eprintln!(
            "skipping: set ASC_DAEMON_TEST_DOCKER=1 to run (needs a live Docker daemon + compose plugin)"
        );
        return;
    }

    let ws = tempfile::tempdir().unwrap();
    let repo = ws.path().join("unsafe-demo");
    fs::create_dir_all(&repo).unwrap();
    fs::write(
        repo.join("docker-compose.yml"),
        "services:\n  web:\n    image: alpine:3.20\n    volumes:\n      - /etc:/etc\n",
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

    let err = pkg::install_from_git(
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
        Some(InstallMethod::DockerCompose),
        None,
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("/etc"), "got: {err:#}");
    assert!(
        !store.app_dir("unsafe-demo").unwrap().exists(),
        "a refused install must not leave a half-created app directory"
    );
}
