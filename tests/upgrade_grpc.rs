//! gRPC-level coverage for `ListAppVersions`/`UpgradeApp`/`UpgradeAppStream`
//! (DMN-0XX/NODE-022): the platform's version picker and danger-zone
//! "update" button go through these, not the CLI's local `pkg::upgrade`
//! call, so they need their own end-to-end check against a real gRPC
//! client. Runs as its own test binary (like install_grpc_license.rs) so
//! nothing here races another test's environment.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use asc_daemon::daemon::api::proto::v1 as pb;
use asc_daemon::daemon::api::{self, ApiState};
use asc_daemon::daemon::apps::{AppStore, UserContext};
use asc_daemon::daemon::config::Config;
use asc_daemon::daemon::pkg::{self, GitRef};

const TOKEN: &str = "test-token-1234";

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

async fn spawn_server(state: Arc<ApiState>) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, api::router(state)).await.unwrap();
    });
    addr
}

fn with_auth<T>(mut request: tonic::Request<T>) -> tonic::Request<T> {
    let value: tonic::metadata::MetadataValue<_> = format!("Bearer {TOKEN}").parse().unwrap();
    request.metadata_mut().insert("authorization", value);
    request
}

async fn channel(addr: std::net::SocketAddr) -> tonic::transport::Channel {
    tonic::transport::Channel::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap()
}

#[tokio::test]
async fn grpc_list_app_versions_returns_tags_newest_first() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: git is not available");
        return;
    }
    let ws = tempfile::tempdir().unwrap();
    let repo = ws.path().join("versioned");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("asc.yaml"), manifest("1.0.0")).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    git(&repo, &["tag", "v1.0.0"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "1.1.0"]);
    git(&repo, &["tag", "v1.1.0"]);
    let repo_url = repo.display().to_string().replace('\\', "/");

    let mut config = Config::default();
    config.daemon.data_dir = ws.path().join("data");
    config.daemon.apps_dir = ws.path().join("apps");
    let state = ApiState::new(config, TOKEN.into());
    let addr = spawn_server(state).await;
    let mut apps = pb::app_service_client::AppServiceClient::new(channel(addr).await);

    let response = apps
        .list_app_versions(with_auth(tonic::Request::new(pb::ListAppVersionsRequest {
            git_url: repo_url,
        })))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(response.tags, vec!["v1.1.0", "v1.0.0"]);
    assert_eq!(response.latest.as_deref(), Some("v1.1.0"));
}

#[tokio::test]
async fn grpc_upgrade_app_moves_to_a_newer_tag_and_reports_up_to_date_otherwise() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: git is not available");
        return;
    }
    let ws = tempfile::tempdir().unwrap();
    let repo = ws.path().join("demo");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("asc.yaml"), manifest("1.0.0")).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    git(&repo, &["tag", "v1.0.0"]);
    let repo_url = repo.display().to_string().replace('\\', "/");

    let mut config = Config::default();
    config.daemon.data_dir = ws.path().join("data");
    config.daemon.apps_dir = ws.path().join("apps");
    let ctx = UserContext {
        uid: 1000,
        name: "tester".into(),
        is_root: false,
    };
    // Seed the app directly (mirrors how other gRPC tests seed state) —
    // the point of this test is UpgradeApp/UpgradeAppStream, not install.
    pkg::install_from_git(
        &config,
        &ctx,
        &repo_url,
        Some(GitRef::Tag("v1.0.0")),
        None,
        None,
        true,
        None,
        None,
    )
    .unwrap();

    let state = ApiState::new(config.clone(), TOKEN.into());
    let addr = spawn_server(state).await;
    let mut apps = pb::app_service_client::AppServiceClient::new(channel(addr).await);

    // Already at the newest tag: up_to_date, no commit churn reported.
    let response = apps
        .upgrade_app(with_auth(tonic::Request::new(pb::UpgradeAppRequest {
            spec: "demo".into(),
        })))
        .await
        .unwrap()
        .into_inner();
    assert!(
        response.up_to_date,
        "expected up_to_date, got: {response:?}"
    );
    assert_eq!(response.to, "v1.0.0");

    // A new release lands in the repository.
    fs::write(repo.join("asc.yaml"), manifest("1.1.0")).unwrap();
    git(&repo, &["commit", "-q", "-am", "1.1.0"]);
    git(&repo, &["tag", "v1.1.0"]);

    // Unary: moves to the new tag, reports both commits, not up_to_date.
    let response = apps
        .upgrade_app(with_auth(tonic::Request::new(pb::UpgradeAppRequest {
            spec: "demo".into(),
        })))
        .await
        .unwrap()
        .into_inner();
    assert!(!response.up_to_date);
    assert_eq!(response.id, "demo");
    assert_eq!(response.from.as_deref(), Some("v1.0.0"));
    assert_eq!(response.to, "v1.1.0");
    assert!(response.from_commit.is_some());
    assert!(response.to_commit.is_some());
    assert_ne!(response.from_commit, response.to_commit);

    let store = AppStore::new(config.daemon.apps_dir.clone());
    assert_eq!(
        store.get("demo").unwrap().unwrap().version.as_deref(),
        Some("v1.1.0")
    );

    // Streaming: another release, watched live — ends in the same terminal
    // result shape, stream closes cleanly (no Err).
    fs::write(repo.join("asc.yaml"), manifest("1.2.0")).unwrap();
    git(&repo, &["commit", "-q", "-am", "1.2.0"]);
    git(&repo, &["tag", "v1.2.0"]);

    let mut stream = apps
        .upgrade_app_stream(with_auth(tonic::Request::new(pb::UpgradeAppRequest {
            spec: "demo".into(),
        })))
        .await
        .unwrap()
        .into_inner();
    let mut terminal = None;
    while let Some(event) = stream.message().await.unwrap() {
        if let Some(pb::upgrade_app_event::Event::Result(result)) = event.event {
            terminal = Some(result);
        }
    }
    let result = terminal.expect("expected a terminal event carrying the upgrade result");
    assert!(!result.up_to_date);
    assert_eq!(result.to, "v1.2.0");
}
