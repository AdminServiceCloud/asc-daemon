//! WebSocket console endpoint: `GET /v1/console?token=<one-time token>`.
//!
//! Deliberately outside the bearer-auth middleware — browsers cannot set
//! headers on WebSocket handshakes. Access is guarded by the one-time
//! console token issued via `IssueConsoleToken` (see docs/console.md):
//! short TTL, single use, bound to one app and session type.
//!
//! Docker apps stream through the Engine API; systemd/process apps stream
//! from a follow-mode subprocess. Attach sessions are multi-client: all
//! connections of one app share a single source through the console hub
//! (see `console::hub`), so several tabs see the same live output.

use std::process::Stdio;
use std::sync::Arc;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::broadcast;
use tracing::{debug, warn};

use super::ApiState;
use super::console::{ConsoleGrant, SessionType};
use crate::daemon::apps::meta::Runtime;
use crate::daemon::config::DockerConfig;
use crate::daemon::{console, docker};

pub fn router(state: Arc<ApiState>) -> Router {
    Router::new()
        .route("/v1/console", get(upgrade))
        .with_state(state)
}

#[derive(Deserialize)]
struct ConsoleQuery {
    #[serde(default)]
    token: String,
    /// Initial log tail (logs sessions).
    #[serde(default)]
    tail: Option<usize>,
}

async fn upgrade(
    ws: WebSocketUpgrade,
    Query(query): Query<ConsoleQuery>,
    State(state): State<Arc<ApiState>>,
) -> Response {
    // One-time token: consumed here, reuse is impossible by construction.
    let Some(grant) = state.console_tokens.consume(&query.token) else {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({ "error": "invalid or expired console token" })),
        )
            .into_response();
    };
    let tail = query.tail.unwrap_or(100);
    ws.on_upgrade(move |socket| handle(socket, state, grant, tail))
}

async fn handle(socket: WebSocket, state: Arc<ApiState>, grant: ConsoleGrant, tail: usize) {
    let (meta, dir) = match load_app(&state, &grant.app_id) {
        Ok(pair) => pair,
        Err(err) => {
            let _ = close_with_error(socket, &format!("{err:#}")).await;
            return;
        }
    };
    let docker_cfg = &state.config.docker;

    let result = match grant.session {
        SessionType::Logs => match &meta.runtime {
            Runtime::Docker { container, .. } => {
                stream_docker_logs(socket, docker_cfg, container, tail).await
            }
            _ => match console::logs_command(&meta, &dir, tail) {
                Ok(cmd) => stream_subprocess_logs(socket, cmd).await,
                Err(err) => close_with_error(socket, &format!("{err:#}")).await,
            },
        },
        SessionType::Attach => match &meta.runtime {
            Runtime::Docker { container, .. } => {
                attach_docker(socket, &state, &grant.app_id, container).await
            }
            other => {
                close_with_error(
                    socket,
                    &format!(
                        "attach is not supported for {} apps yet (docker only)",
                        other.kind()
                    ),
                )
                .await
            }
        },
    };
    if let Err(err) = result {
        debug!(app = %grant.app_id, error = %format!("{err:#}"), "console session ended with error");
    }
}

fn load_app(
    state: &ApiState,
    id: &str,
) -> anyhow::Result<(crate::daemon::apps::AppMeta, std::path::PathBuf)> {
    // The grant was issued after an authorization check; reload the meta in
    // case the app changed between token issue and use.
    let meta = state.manager.get_authorized(&super::api_context(), id)?;
    let dir = state.manager.store().app_dir(&meta.id)?;
    Ok((meta, dir))
}

async fn close_with_error(mut socket: WebSocket, message: &str) -> anyhow::Result<()> {
    socket
        .send(Message::Text(format!("error: {message}").into()))
        .await
        .ok();
    socket.close().await.ok();
    Ok(())
}

/// Docker logs (read-only) via the Engine API → text frames.
async fn stream_docker_logs(
    mut socket: WebSocket,
    cfg: &DockerConfig,
    container: &str,
    tail: usize,
) -> anyhow::Result<()> {
    let log_stream = match docker::logs_follow(cfg, container, tail).await {
        Ok(stream) => stream,
        Err(err) => return close_with_error(socket, &format!("{err:#}")).await,
    };
    tokio::pin!(log_stream);
    loop {
        tokio::select! {
            line = log_stream.next() => match line {
                Some(Ok(line)) => socket.send(Message::Text(line.into())).await?,
                Some(Err(err)) => {
                    socket.send(Message::Text(format!("error: {err:#}").into())).await.ok();
                    break;
                }
                None => break, // container stopped / log stream ended
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {} // logs are read-only: ignore client input
                Some(Err(err)) => {
                    warn!(error = %err, "console socket error");
                    break;
                }
            },
        }
    }
    socket.close().await.ok();
    Ok(())
}

/// Docker attach (bidirectional): client frames → stdin, container → binary
/// frames. All clients of one app share a source through the console hub:
/// this connection joins it, replays recent output, then follows live.
async fn attach_docker(
    mut socket: WebSocket,
    state: &ApiState,
    app_id: &str,
    container: &str,
) -> anyhow::Result<()> {
    let cfg = &state.config.docker;
    let connect = async {
        let attach = docker::attach(cfg, container).await?;
        let output = attach.output.map(|item| {
            item.map(|chunk| chunk.into_bytes().to_vec())
                .map_err(|e| anyhow::anyhow!("docker attach: {e}"))
        });
        Ok((output, attach.input))
    };
    let mut client = match state.attach_hub.subscribe(app_id, connect).await {
        Ok(client) => client,
        Err(err) => return close_with_error(socket, &format!("{err:#}")).await,
    };
    for chunk in std::mem::take(&mut client.replay) {
        socket.send(Message::Binary(chunk.into())).await?;
    }
    loop {
        tokio::select! {
            output = client.rx.recv() => match output {
                Ok(chunk) => socket.send(Message::Binary(chunk.into())).await?,
                // This client fell behind the fan-out and lost the oldest
                // chunks; the live stream continues.
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break, // app stopped
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Binary(data))) => client.session.send_stdin(data.to_vec()).await?,
                Some(Ok(Message::Text(text))) => {
                    client.session.send_stdin(text.as_bytes().to_vec()).await?
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(err)) => {
                    warn!(error = %err, "console socket error");
                    break;
                }
            },
        }
    }
    socket.close().await.ok();
    Ok(())
}

/// Read-only log stream from a follow-mode subprocess (systemd/process).
/// The child is killed when the client disconnects.
async fn stream_subprocess_logs(
    mut socket: WebSocket,
    mut cmd: tokio::process::Command,
) -> anyhow::Result<()> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let mut out_lines = BufReader::new(stdout).lines();
    let mut err_lines = BufReader::new(stderr).lines();

    loop {
        tokio::select! {
            line = out_lines.next_line() => match line? {
                Some(line) => socket.send(Message::Text(line.into())).await?,
                None => break, // log source finished
            },
            line = err_lines.next_line() => {
                if let Some(line) = line? {
                    socket.send(Message::Text(line.into())).await?;
                }
            }
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(err)) => {
                    warn!(error = %err, "console socket error");
                    break;
                }
            },
        }
    }
    child.kill().await.ok();
    socket.close().await.ok();
    Ok(())
}
