//! REST transport (JSON over HTTP) — the same operations as the gRPC
//! services, mapped onto resource routes (see docs/api.md). Field names and
//! semantics mirror the proto messages so the two transports never diverge.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use super::ApiState;
use super::console::SessionType;
use crate::daemon::apps::{AppStatus, Outcome, RuntimeState};
use crate::daemon::pkg::InstallOutcome;

pub fn router(state: Arc<ApiState>) -> Router {
    Router::new()
        .route("/v1/status", get(status))
        .route("/v1/metrics", get(system_metrics))
        .route("/v1/metrics/history", get(metrics_history))
        .route("/v1/apps", get(list_apps).post(install_app))
        .route("/v1/apps/{id}", get(get_app).delete(remove_app))
        .route("/v1/apps/{id}/start", post(start_app))
        .route("/v1/apps/{id}/stop", post(stop_app))
        .route("/v1/apps/{id}/restart", post(restart_app))
        .route("/v1/apps/{id}/logs", get(app_logs))
        .route("/v1/apps/{id}/console-token", post(console_token))
        .with_state(state)
}

/// anyhow errors → JSON error responses (404 for missing apps/packages).
struct ApiError(anyhow::Error);

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        Self(err)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let msg = format!("{:#}", self.0);
        let code = if msg.contains("not found") || msg.contains("не найдено") {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        (code, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}

#[derive(Serialize)]
struct AppJson {
    id: String,
    name: String,
    kind: &'static str,
    state: &'static str,
    version: Option<String>,
    source: Option<String>,
    owner: String,
}

fn to_json(status: &AppStatus) -> AppJson {
    AppJson {
        id: status.meta.id.clone(),
        name: status.meta.name.clone(),
        kind: status.meta.runtime.kind(),
        state: match status.state {
            RuntimeState::Running => "running",
            RuntimeState::Stopped => "stopped",
        },
        version: status.meta.version.clone(),
        source: status.meta.source.clone(),
        owner: status.meta.owner.name.clone(),
    }
}

async fn status(State(state): State<Arc<ApiState>>) -> Result<Response, ApiError> {
    let (running, total) = state.status().await?;
    Ok(Json(serde_json::json!({
        "version": crate::VERSION,
        "apps_total": total,
        "apps_running": running,
    }))
    .into_response())
}

/// Flat JSON mirroring `SystemMetrics` in the proto, so REST and gRPC
/// consumers see identical field names.
fn metrics_json(m: &crate::daemon::monitor::SystemMetrics) -> serde_json::Value {
    serde_json::json!({
        "timestamp": m.timestamp,
        "cpu_usage_percent": m.cpu.usage_percent,
        "cpu_cores": m.cpu.cores,
        "load1": m.cpu.load1,
        "load5": m.cpu.load5,
        "load15": m.cpu.load15,
        "mem_total": m.memory.total,
        "mem_used": m.memory.used,
        "mem_available": m.memory.available,
        "swap_total": m.memory.swap_total,
        "swap_used": m.memory.swap_used,
        "uptime_secs": m.uptime_secs,
        "disks": m.disks.iter().map(|d| serde_json::json!({
            "mount": d.mount,
            "filesystem": d.filesystem,
            "total": d.total,
            "used": d.used,
            "available": d.available,
        })).collect::<Vec<_>>(),
        "network": m.network.iter().map(|n| serde_json::json!({
            "interface": n.interface,
            "rx_bytes": n.rx_bytes,
            "tx_bytes": n.tx_bytes,
            "rx_errors": n.rx_errors,
            "tx_errors": n.tx_errors,
            "rx_bytes_per_sec": n.rx_bytes_per_sec,
            "tx_bytes_per_sec": n.tx_bytes_per_sec,
        })).collect::<Vec<_>>(),
    })
}

async fn system_metrics(State(state): State<Arc<ApiState>>) -> Response {
    match state.monitor.latest() {
        Some(m) => Json(serde_json::json!({ "metrics": metrics_json(&m) })).into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no metrics samples yet, retry shortly" })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct HistoryQuery {
    /// Maximum number of most recent samples; 0 or absent = the whole buffer.
    #[serde(default)]
    limit: usize,
}

async fn metrics_history(
    State(state): State<Arc<ApiState>>,
    Query(query): Query<HistoryQuery>,
) -> Response {
    let samples: Vec<_> = state
        .monitor
        .history(query.limit)
        .iter()
        .map(metrics_json)
        .collect();
    Json(serde_json::json!({ "samples": samples })).into_response()
}

async fn list_apps(State(state): State<Arc<ApiState>>) -> Result<Response, ApiError> {
    let apps = state.list_apps().await?;
    let apps: Vec<AppJson> = apps.iter().map(to_json).collect();
    Ok(Json(serde_json::json!({ "apps": apps })).into_response())
}

async fn get_app(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let status = state.get_app(id).await?;
    Ok(Json(serde_json::json!({ "app": to_json(&status) })).into_response())
}

#[derive(Deserialize)]
struct InstallBody {
    /// "name", "stack" or "stack/app", optionally with "@version".
    spec: String,
}

async fn install_app(
    State(state): State<Arc<ApiState>>,
    Json(body): Json<InstallBody>,
) -> Result<Response, ApiError> {
    // Mirrors InstallAppResponse from the proto contract.
    let json = match state.install(body.spec).await? {
        InstallOutcome::App(report) => serde_json::json!({
            "id": report.id,
            "version": report.version,
            "apps": [],
            "skipped": [],
        }),
        InstallOutcome::Stack {
            stack,
            installed,
            skipped,
        } => serde_json::json!({
            "id": stack,
            "version": installed.first().map(|r| r.version.clone()).unwrap_or_default(),
            "apps": installed
                .iter()
                .map(|r| serde_json::json!({ "id": r.id, "version": r.version }))
                .collect::<Vec<_>>(),
            "skipped": skipped,
        }),
    };
    Ok((StatusCode::CREATED, Json(json)).into_response())
}

async fn start_app(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let outcome = state.start(id).await?;
    Ok(Json(serde_json::json!({
        "already_running": outcome == Outcome::AlreadyInState
    }))
    .into_response())
}

async fn stop_app(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let outcome = state.stop(id).await?;
    Ok(Json(serde_json::json!({
        "already_stopped": outcome == Outcome::AlreadyInState
    }))
    .into_response())
}

async fn restart_app(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    state.restart(id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct LogsQuery {
    #[serde(default)]
    tail: Option<usize>,
}

async fn app_logs(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<String>,
    Query(query): Query<LogsQuery>,
) -> Result<Response, ApiError> {
    let logs = state.logs(id, query.tail.unwrap_or(100)).await?;
    Ok(Json(serde_json::json!({ "logs": logs })).into_response())
}

async fn remove_app(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    state.remove(id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct ConsoleTokenBody {
    /// "logs" or "attach" — mirrors ConsoleSessionType in the proto.
    session: String,
}

async fn console_token(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<String>,
    Json(body): Json<ConsoleTokenBody>,
) -> Result<Response, ApiError> {
    let session = match body.session.as_str() {
        "logs" => SessionType::Logs,
        "attach" => SessionType::Attach,
        _ => {
            return Ok((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "session must be 'logs' or 'attach'" })),
            )
                .into_response());
        }
    };
    let (token, expires_at) = state.issue_console_token(id, session).await?;
    Ok(Json(serde_json::json!({ "token": token, "expires_at": expires_at })).into_response())
}
