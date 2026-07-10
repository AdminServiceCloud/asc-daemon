//! WebSocket console integration: one-time token gate + live log streaming
//! against a real in-process server.

use std::fs;
use std::sync::Arc;

use asc_daemon::daemon::api::console::SessionType;
use asc_daemon::daemon::api::{self, ApiState};
use asc_daemon::daemon::apps::AppStore;
use asc_daemon::daemon::apps::meta::{AppMeta, DesiredState, Owner, Runtime};
use asc_daemon::daemon::config::Config;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

fn test_state() -> (Arc<ApiState>, tempfile::TempDir) {
    let ws = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.daemon.data_dir = ws.path().join("data");
    config.daemon.apps_dir = ws.path().join("apps");
    (ApiState::new(config, "api-token".into()), ws)
}

async fn spawn_server(state: Arc<ApiState>) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, api::router(state)).await.unwrap();
    });
    addr
}

fn install_app_with_logs(state: &ApiState, id: &str, log_lines: &str) {
    let store = AppStore::new(state.config.daemon.apps_dir.clone());
    store
        .save(&AppMeta {
            id: id.into(),
            name: id.into(),
            owner: Owner {
                uid: 0,
                name: "root".into(),
            },
            version: None,
            source: None,
            desired_state: DesiredState::Stopped,
            quota: None,
            runtime: Runtime::Process {
                command: "true".into(),
                args: vec![],
            },
        })
        .unwrap();
    fs::write(store.app_dir(id).unwrap().join("app.log"), log_lines).unwrap();
}

#[tokio::test]
async fn ws_streams_logs_with_valid_token() {
    let (state, _ws) = test_state();
    install_app_with_logs(&state, "demo", "line-one\nline-two\n");
    let addr = spawn_server(Arc::clone(&state)).await;

    let (token, _) = state
        .issue_console_token("demo".into(), SessionType::Logs)
        .await
        .unwrap();

    let (mut socket, _) = connect_async(format!("ws://{addr}/v1/console?token={token}"))
        .await
        .expect("handshake with a valid token");

    let mut received = Vec::new();
    while received.len() < 2 {
        match socket.next().await.expect("stream open").unwrap() {
            Message::Text(line) => received.push(line),
            Message::Close(_) => break,
            _ => {}
        }
    }
    assert_eq!(received, ["line-one", "line-two"]);
    socket.send(Message::Close(None)).await.ok();
}

#[tokio::test]
async fn ws_rejects_invalid_and_reused_tokens() {
    let (state, _ws) = test_state();
    install_app_with_logs(&state, "demo", "x\n");
    let addr = spawn_server(Arc::clone(&state)).await;

    // Garbage token → handshake refused (401).
    let err = connect_async(format!("ws://{addr}/v1/console?token=deadbeef")).await;
    assert!(err.is_err(), "invalid token must not connect");

    // A valid token works once...
    let (token, _) = state
        .issue_console_token("demo".into(), SessionType::Logs)
        .await
        .unwrap();
    let ok = connect_async(format!("ws://{addr}/v1/console?token={token}")).await;
    assert!(ok.is_ok());

    // ...and is burned after the first handshake.
    let reused = connect_async(format!("ws://{addr}/v1/console?token={token}")).await;
    assert!(reused.is_err(), "console tokens are single-use");
}

#[tokio::test]
async fn attach_reports_unsupported_runtime() {
    let (state, _ws) = test_state();
    install_app_with_logs(&state, "demo", "");
    let addr = spawn_server(Arc::clone(&state)).await;

    let (token, _) = state
        .issue_console_token("demo".into(), SessionType::Attach)
        .await
        .unwrap();
    let (mut socket, _) = connect_async(format!("ws://{addr}/v1/console?token={token}"))
        .await
        .unwrap();
    // Process apps cannot attach yet: the server says so and closes.
    match socket.next().await.expect("one frame").unwrap() {
        Message::Text(text) => assert!(text.contains("attach is not supported")),
        other => panic!("expected error text frame, got {other:?}"),
    }
}
