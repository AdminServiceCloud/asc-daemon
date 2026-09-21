//! `CloneApp`/`CloneAppStream` over gRPC (DMN-113): the platform's clone
//! dialog calls these to copy an installed app under a new id — see
//! tests/clone.rs for the service-layer behavior this wraps.

use std::fs;
use std::sync::Arc;

use asc_daemon::daemon::api::proto::v1 as pb;
use asc_daemon::daemon::api::{self, ApiState};
use asc_daemon::daemon::apps::AppStore;
use asc_daemon::daemon::apps::meta::{AppMeta, DesiredState, Owner, Runtime};
use asc_daemon::daemon::config::Config;

const TOKEN: &str = "test-token-1234";

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

/// Builds a source app directly on disk, the same way tests/clone.rs does —
/// `locate_installed`'s fast path (an asc.yaml right at the repository root)
/// needs no registry, so this is enough for a real clone.
fn seed_app(store: &AppStore, id: &str) -> AppMeta {
    let app_dir = store.app_dir(id).unwrap();
    fs::create_dir_all(app_dir.join("repository")).unwrap();
    fs::write(
        app_dir.join("repository/asc.yaml"),
        "name: demo\nversion: 1.0.0\ntype: native\nruntime:\n  start: ./run.sh\n",
    )
    .unwrap();
    fs::create_dir_all(app_dir.join("config")).unwrap();
    fs::create_dir_all(app_dir.join("data")).unwrap();
    fs::write(app_dir.join("data/save.txt"), b"progress=42").unwrap();

    let meta = AppMeta {
        id: id.to_string(),
        uuid: None,
        name: "Demo".into(),
        custom_name: None,
        owner: Owner {
            uid: 0,
            name: "root".into(),
        },
        version: Some("1.0.0".into()),
        source: Some("local:file:///demo".into()),
        branch: None,
        repo_path: None,
        package: None,
        desired_state: DesiredState::Stopped,
        quota: None,
        runtime: Runtime::Process {
            command: "/bin/sh".into(),
            args: vec!["-c".into(), "./run.sh".into()],
        },
    };
    store.save(&meta).unwrap();
    meta
}

#[tokio::test]
async fn grpc_clone_app_copies_under_a_new_id() {
    let ws = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.daemon.data_dir = ws.path().join("data");
    config.daemon.apps_dir = ws.path().join("apps");
    let store = AppStore::new(config.daemon.apps_dir.clone());
    seed_app(&store, "demo");

    let state = ApiState::new(config, TOKEN.into());
    let addr = spawn_server(state).await;
    let mut apps = pb::app_service_client::AppServiceClient::new(channel(addr).await);

    let response = apps
        .clone_app(with_auth(tonic::Request::new(pb::CloneAppRequest {
            id: "demo".into(),
            name: None,
        })))
        .await
        .unwrap()
        .into_inner();

    assert_eq!(response.id, "demo-2");
    assert_eq!(response.name.as_deref(), Some("demo-2"));
    assert!(response.copied_bytes > 0);

    let clone_dir = ws.path().join("apps/demo-2");
    assert_eq!(
        fs::read_to_string(clone_dir.join("data/save.txt")).unwrap(),
        "progress=42"
    );
    // The source is untouched.
    assert!(ws.path().join("apps/demo/data/save.txt").exists());
}

#[tokio::test]
async fn grpc_clone_app_stream_reports_progress_and_a_custom_name() {
    let ws = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.daemon.data_dir = ws.path().join("data");
    config.daemon.apps_dir = ws.path().join("apps");
    let store = AppStore::new(config.daemon.apps_dir.clone());
    seed_app(&store, "demo");

    let state = ApiState::new(config, TOKEN.into());
    let addr = spawn_server(state).await;
    let mut apps = pb::app_service_client::AppServiceClient::new(channel(addr).await);

    let mut stream = apps
        .clone_app_stream(with_auth(tonic::Request::new(pb::CloneAppRequest {
            id: "demo".into(),
            name: Some("demo-backup".into()),
        })))
        .await
        .unwrap()
        .into_inner();

    let mut lines = Vec::new();
    let mut result = None;
    while let Some(event) = stream.message().await.unwrap() {
        match event.event {
            Some(pb::clone_app_event::Event::Line(line)) => lines.push(line),
            Some(pb::clone_app_event::Event::Result(response)) => result = Some(response),
            None => {}
        }
    }

    // At least one progress line arrived (the copy reaches 100% before the
    // terminal result, and copy_tree copies more than zero bytes here).
    assert!(!lines.is_empty(), "expected at least one progress line");
    let result = result.expect("stream must end with a result event");
    assert_eq!(result.id, "demo-2");
    assert_eq!(result.name.as_deref(), Some("demo-backup"));
    assert!(result.copied_bytes > 0);
}
