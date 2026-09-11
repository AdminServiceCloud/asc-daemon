//! DMN-091: the gRPC install RPCs must not fail with a bare error when the
//! package repository ships a LICENSE — the platform's install dialog has no
//! way to render an opaque error string as a consent screen. Runs as its own
//! test binary (like install.rs) so the ASC_SOURCES/ASC_USER_SOURCES env vars
//! it sets do not race with other tests.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use asc_daemon::daemon::api::proto::v1 as pb;
use asc_daemon::daemon::api::{self, ApiState};
use asc_daemon::daemon::config::Config;
use asc_daemon::daemon::pkg::registry::file_source_url;

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

/// A repository shipping a LICENSE, registered under a local `file://`
/// registry — mirrors the setup in tests/install.rs. Registry-spec installs
/// (not a direct git URL) go through the same `pkg::install` →
/// `require_license_ack` path the direct-git dispatch does, so this equally
/// exercises the gRPC boundary's `license_required` handling.
#[tokio::test]
async fn grpc_install_returns_license_required_instead_of_an_error() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: git is not available");
        return;
    }
    let ws = tempfile::tempdir().unwrap();

    let repo = ws.path().join("licensed-repo");
    fs::create_dir_all(&repo).unwrap();
    fs::write(
        repo.join("asc.yaml"),
        "name: licensed\nversion: 1.0.0\ntype: native\nruntime:\n  start: ./run.sh\n",
    )
    .unwrap();
    fs::write(repo.join("LICENSE.md"), "MIT License\n\nDemo terms.\n").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    git(&repo, &["tag", "v1.0.0"]);
    let repo_url = repo.display().to_string().replace('\\', "/");

    let reg = ws.path().join("registry");
    fs::create_dir_all(reg.join("categories")).unwrap();
    fs::write(
        reg.join("registry.json"),
        r#"{"name":"local","categories":[{"name":"web","index":"categories/web.json"}]}"#,
    )
    .unwrap();
    fs::write(
        reg.join("categories/web.json"),
        format!(
            r#"{{"category":"web","packages":[{{"name":"licensed","type":"app","description":"Demo","source":{{"git":"{repo_url}"}}}}]}}"#
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
    let state = ApiState::new(config, TOKEN.into());
    let addr = spawn_server(state).await;

    let mut apps = pb::app_service_client::AppServiceClient::new(channel(addr).await);
    let request = |license_ack: bool| pb::InstallAppRequest {
        spec: "licensed".into(),
        source: String::new(),
        name: None,
        branch: None,
        tag: None,
        license_ack,
    };

    // Unary: license required comes back as a normal response, not a gRPC
    // error — no status code to check here at all.
    let response = apps
        .install_app(with_auth(tonic::Request::new(request(false))))
        .await
        .unwrap()
        .into_inner();
    let required = response
        .license_required
        .expect("expected license_required to be set");
    assert_eq!(required.package, "licensed");
    assert_eq!(required.source, "local");
    assert_eq!(required.git, repo_url);
    assert!(required.license.contains("MIT License"));
    assert_eq!(response.id, "", "nothing should be reported installed");

    // Streaming: same outcome as the stream's terminal event; the stream
    // ends cleanly rather than with an `Err`.
    let mut stream = apps
        .install_app_stream(with_auth(tonic::Request::new(request(false))))
        .await
        .unwrap()
        .into_inner();
    let mut terminal = None;
    while let Some(event) = stream.message().await.unwrap() {
        if let Some(pb::install_app_event::Event::Result(result)) = event.event {
            terminal = Some(result);
        }
    }
    let required = terminal
        .and_then(|result| result.license_required)
        .expect("expected a terminal event carrying license_required");
    assert_eq!(required.git, repo_url);

    // Accepted: installs normally, id/version populated, no
    // license_required.
    let response = apps
        .install_app(with_auth(tonic::Request::new(request(true))))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(response.id, "licensed");
    assert!(response.license_required.is_none());
}

async fn channel(addr: std::net::SocketAddr) -> tonic::transport::Channel {
    tonic::transport::Channel::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap()
}
