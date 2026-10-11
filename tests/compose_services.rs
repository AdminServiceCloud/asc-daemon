//! Services of a docker compose app: list, control, logs, settings and the
//! per-service console. Gated behind `ASC_DAEMON_TEST_DOCKER=1` since it
//! needs a live Docker daemon **and** the `docker compose` CLI plugin —
//! mirrors `tests/install_compose.rs`.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use asc_daemon::daemon::api::console::SessionType;
use asc_daemon::daemon::api::{self, ApiState};
use asc_daemon::daemon::apps::compose_override::ServiceOverride;
use asc_daemon::daemon::apps::compose_services::ServiceAction;
use asc_daemon::daemon::apps::meta::DesiredState;
use asc_daemon::daemon::apps::{AppManager, AppStore, RuntimeState, UserContext};
use asc_daemon::daemon::config::Config;
use asc_daemon::daemon::docker;
use asc_daemon::daemon::exec::ModuleError;
use asc_daemon::daemon::pkg::{self, GitRef, InstallMethod, UpgradeOutcome};
use futures_util::StreamExt;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

fn docker_enabled() -> bool {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: git is not available");
        return false;
    }
    if std::env::var("ASC_DAEMON_TEST_DOCKER").as_deref() != Ok("1") {
        eprintln!(
            "skipping: set ASC_DAEMON_TEST_DOCKER=1 to run (needs a live Docker daemon + compose plugin)"
        );
        return false;
    }
    true
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

fn root() -> UserContext {
    UserContext {
        uid: 0,
        name: "root".into(),
        is_root: true,
    }
}

const COMPOSE: &str = "services:
  web:
    image: alpine:3.20
    command: [\"sh\", \"-c\", \"echo hello-from-web; sleep 3600\"]
  worker:
    image: alpine:3.20
    command: [\"sh\", \"-c\", \"echo hello-from-worker; sleep 3600\"]
";

fn kind(err: &anyhow::Error) -> Option<&ModuleError> {
    err.downcast_ref::<ModuleError>()
}

/// `docker exec` through bollard, off the test's async runtime (the
/// synchronous helpers run their own).
async fn exec_in(config: &Config, container: &str, script: &str) -> (i64, String) {
    let (docker_cfg, container, script) = (
        config.docker.clone(),
        container.to_string(),
        script.to_string(),
    );
    tokio::task::spawn_blocking(move || {
        docker::exec_collect(&docker_cfg, &container, &["sh".into(), "-c".into(), script]).unwrap()
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn services_list_control_logs_settings_and_console() {
    if !docker_enabled() {
        return;
    }
    let ws = tempfile::tempdir().unwrap();
    let repo = ws.path().join("csvc-demo");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("docker-compose.yml"), COMPOSE).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let url = repo.display().to_string().replace('\\', "/");

    let mut config = Config::default();
    config.daemon.data_dir = ws.path().join("data");
    config.daemon.apps_dir = ws.path().join("apps");
    let state = ApiState::new(config.clone(), "api-token".into());
    let manager = AppManager::new(&config);
    let store = AppStore::new(config.daemon.apps_dir.clone());

    let (cfg, url2) = (config.clone(), url.clone());
    let id = tokio::task::spawn_blocking(move || {
        match pkg::install_from_git(
            &cfg,
            &root(),
            &url2,
            Some(GitRef::Branch("main")),
            None,
            None,
            None,
            true,
            None,
            false,
            Some(InstallMethod::DockerCompose),
            None,
        )
        .unwrap()
        {
            pkg::InstallOutcome::App(report) => report.id,
            other => panic!("expected an app install, got {other:?}"),
        }
    })
    .await
    .unwrap();

    let body = async {
        // Created, not started: every service has a container, none runs.
        let listed = state.list_apps(root()).await.unwrap();
        let app = listed.iter().find(|a| a.meta.id == id).unwrap();
        assert_eq!(app.state, RuntimeState::Stopped);
        let names: Vec<&str> = app
            .compose_services
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, ["web", "worker"]);
        assert!(app.compose_services.iter().all(|s| s.state == "created"));

        state.start(root(), id.clone()).await.unwrap();
        let services = state
            .list_app_services(root(), id.clone(), None, true)
            .await
            .unwrap();
        assert_eq!(services.len(), 2);
        for (service, stats) in &services {
            assert_eq!(service.state, "running", "{}", service.name);
            assert_eq!(service.replicas, 1);
            assert!(!service.buildable);
            assert!(service.started_at.is_some());
            assert!(stats.is_some(), "stats of {}", service.name);
        }

        // Stopping one service is temporary: the app stays running and
        // keeps wanting to run.
        let worker = state
            .control_app_service(root(), id.clone(), "worker".into(), ServiceAction::Stop)
            .await
            .unwrap();
        assert_eq!(worker.state, "exited");
        let status = state.get_app(root(), id.clone()).await.unwrap();
        assert_eq!(status.state, RuntimeState::Running);
        assert_eq!(status.meta.desired_state, DesiredState::Running);
        let web_before = services[0].0.container_id.clone();
        let worker = state
            .control_app_service(root(), id.clone(), "worker".into(), ServiceAction::Start)
            .await
            .unwrap();
        assert_eq!(worker.state, "running");
        let web = state
            .control_app_service(root(), id.clone(), "web".into(), ServiceAction::Restart)
            .await
            .unwrap();
        assert_eq!(web.state, "running");
        assert_eq!(
            web.container_id, web_before,
            "a restart keeps the container"
        );

        // One service's log, without the service-name prefix.
        let logs = state
            .logs(root(), id.clone(), 50, false, Some("web".into()))
            .await
            .unwrap();
        assert!(logs.contains("hello-from-web"), "{logs}");
        assert!(!logs.contains("hello-from-worker"), "{logs}");
        assert!(!logs.contains("web-1"), "{logs}");

        // Unknown service, wrong kind of request.
        let err = state
            .control_app_service(root(), id.clone(), "nope".into(), ServiceAction::Stop)
            .await
            .unwrap_err();
        assert!(
            matches!(kind(&err), Some(ModuleError::NotFound(_))),
            "{err:#}"
        );

        // Settings: validated, written to the override file, applied.
        let mut bad = ServiceOverride::default();
        bad.environment.insert("1BAD".into(), "x".into());
        let err = state
            .set_app_service_settings(root(), id.clone(), "web".into(), bad, true)
            .await
            .unwrap_err();
        assert!(
            matches!(kind(&err), Some(ModuleError::Invalid(_))),
            "{err:#}"
        );
        let overrides = ServiceOverride {
            environment: BTreeMap::from([("ASC_SVC".to_string(), "on $HOME".to_string())]),
            memory_bytes: Some(64 << 20),
            restart: Some("unless-stopped".into()),
            ..Default::default()
        };
        let write = state
            .set_app_service_settings(root(), id.clone(), "web".into(), overrides.clone(), true)
            .await
            .unwrap();
        assert!(write.applied && !write.restart_required);
        let settings = state
            .app_service_settings(root(), id.clone(), "web".into())
            .await
            .unwrap();
        assert_eq!(settings.overrides, overrides);
        assert!(
            settings
                .effective_environment_keys
                .contains(&"ASC_SVC".to_string())
        );
        assert_eq!(settings.effective_memory_bytes, Some(64 << 20));
        assert_eq!(
            settings.effective_restart.as_deref(),
            Some("unless-stopped")
        );
        let services = state
            .list_app_services(root(), id.clone(), Some("web".into()), false)
            .await
            .unwrap();
        let web = &services[0].0;
        assert!(web.has_overrides);
        assert_ne!(web.container_id, web_before, "applied by recreating web");
        let (code, out) = exec_in(&config, &web.container_id, "echo \"$ASC_SVC\"").await;
        assert_eq!((code, out.trim()), (0, "on $HOME"));

        // The console of one service: its own log, and a shell in it.
        let addr = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let router = api::router(Arc::clone(&state));
            tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            addr
        };
        let (token, _) = state
            .issue_console_token(
                root(),
                id.clone(),
                SessionType::Logs,
                Vec::new(),
                Some("worker".into()),
            )
            .await
            .unwrap();
        let (mut socket, _) = connect_async(format!("ws://{addr}/v1/console?token={token}"))
            .await
            .unwrap();
        let frame = tokio::time::timeout(std::time::Duration::from_secs(10), socket.next())
            .await
            .expect("a log line in time")
            .unwrap()
            .unwrap();
        match frame {
            Message::Text(text) => assert!(text.contains("hello-from-worker"), "{text}"),
            other => panic!("expected a log line, got {other:?}"),
        }
        drop(socket);

        let (token, _) = state
            .issue_console_token(
                root(),
                id.clone(),
                SessionType::Exec,
                vec!["sh".into(), "-c".into(), "echo exec-in-web".into()],
                Some("web".into()),
            )
            .await
            .unwrap();
        let (mut socket, _) = connect_async(format!("ws://{addr}/v1/console?token={token}"))
            .await
            .unwrap();
        let mut output = Vec::new();
        while let Ok(Some(Ok(frame))) =
            tokio::time::timeout(std::time::Duration::from_secs(10), socket.next()).await
        {
            match frame {
                Message::Binary(data) if !data.is_empty() => output.extend_from_slice(&data[1..]),
                Message::Close(_) => break,
                Message::Text(text) => panic!("exec failed: {text}"),
                _ => {}
            }
            if String::from_utf8_lossy(&output).contains("exec-in-web") {
                break;
            }
        }
        assert!(
            String::from_utf8_lossy(&output).contains("exec-in-web"),
            "{}",
            String::from_utf8_lossy(&output)
        );

        // No shell on the project as a whole, no attach to one service.
        let err = state
            .issue_console_token(root(), id.clone(), SessionType::Exec, Vec::new(), None)
            .await
            .unwrap_err();
        assert!(
            matches!(kind(&err), Some(ModuleError::Precondition(_))),
            "{err:#}"
        );
        let err = state
            .issue_console_token(
                root(),
                id.clone(),
                SessionType::Attach,
                Vec::new(),
                Some("web".into()),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(kind(&err), Some(ModuleError::Precondition(_))),
            "{err:#}"
        );

        // The override lives outside the repository and survives an upgrade
        // that pulls a new commit.
        fs::write(repo.join("README.md"), "v2\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "v2"]);
        let outcome = state
            .upgrade(root(), id.clone(), pkg::UpgradeRef::Spec)
            .await
            .unwrap();
        assert!(
            matches!(outcome, UpgradeOutcome::Upgraded { .. }),
            "{outcome:?}"
        );
        let app_dir = store.app_dir(&id).unwrap();
        assert!(asc_daemon::daemon::apps::compose::override_path(&app_dir).is_file());
        let services = state
            .list_app_services(root(), id.clone(), Some("web".into()), false)
            .await
            .unwrap();
        let (code, out) = exec_in(&config, &services[0].0.container_id, "echo \"$ASC_SVC\"").await;
        assert_eq!((code, out.trim()), (0, "on $HOME"));
    };
    let result = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(body)).await;
    let cleanup_id = id.clone();
    tokio::task::spawn_blocking(move || {
        let _ = manager.remove(&root(), &cleanup_id);
    })
    .await
    .unwrap();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
