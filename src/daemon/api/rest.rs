//! REST transport (JSON over HTTP) — the same operations as the gRPC
//! services, mapped onto resource routes (see docs/api.md). Field names and
//! semantics mirror the proto messages so the two transports never diverge.

use std::sync::Arc;

use axum::Extension;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use super::ApiState;
use super::console::SessionType;
use crate::daemon::apps::{AppStatus, Outcome, RuntimeState, UserContext};
use crate::daemon::pkg::InstallOutcome;

pub fn router(state: Arc<ApiState>) -> Router {
    Router::new()
        .route("/v1/status", get(status))
        .route("/v1/metrics", get(system_metrics))
        .route("/v1/metrics/history", get(metrics_history))
        .route("/v1/apps", get(list_apps).post(install_app))
        .route("/v1/apps/{id}", get(get_app).delete(remove_app))
        .route("/v1/apps/{id}/disk", get(app_disk))
        .route("/v1/apps/{id}/start", post(start_app))
        .route("/v1/apps/{id}/stop", post(stop_app))
        .route("/v1/apps/{id}/restart", post(restart_app))
        .route("/v1/apps/{id}/logs", get(app_logs))
        .route("/v1/apps/{id}/console-token", post(console_token))
        .with_state(state)
}

/// anyhow errors → JSON error responses (404 for missing apps/packages).
///
/// The typed install errors keep their structure (DMN-028/DMN-042): a
/// client that can act on them — the CLI's consent prompt, the platform
/// UI's dialog — reads the payload instead of parsing the message.
struct ApiError(anyhow::Error);

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        Self(err)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let msg = format!("{:#}", self.0);
        if let Some(required) = self.0.downcast_ref::<crate::daemon::pkg::LicenseRequired>() {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": msg,
                    "license_required": {
                        "package": required.package,
                        "source": required.source,
                        "git": required.git,
                        "license": required.license,
                    },
                })),
            )
                .into_response();
        }
        if let Some(ambiguous) = self
            .0
            .downcast_ref::<crate::daemon::pkg::AmbiguousPackage>()
        {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": msg,
                    "ambiguous": {
                        "name": ambiguous.name,
                        "candidates": ambiguous.candidates.iter().map(|(source, git)| {
                            serde_json::json!({ "source": source, "git": git })
                        }).collect::<Vec<_>>(),
                    },
                })),
            )
                .into_response();
        }
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
    /// The package title when the app carries a custom name (then `name`
    /// is the custom name).
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    quota: Option<crate::daemon::apps::meta::Quota>,
}

fn to_json(status: &AppStatus) -> AppJson {
    AppJson {
        id: status.meta.id.clone(),
        name: status.meta.display_name().to_string(),
        kind: status.meta.runtime.kind(),
        state: match status.state {
            RuntimeState::Running => "running",
            RuntimeState::Stopped => "stopped",
        },
        version: status.meta.version.clone(),
        source: status.meta.source.clone(),
        owner: status.meta.owner.name.clone(),
        title: status
            .meta
            .custom_name
            .is_some()
            .then(|| status.meta.name.clone()),
        quota: status.meta.quota,
    }
}

async fn status(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
) -> Result<Response, ApiError> {
    let (running, total) = state.status(ctx).await?;
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

async fn list_apps(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
) -> Result<Response, ApiError> {
    let apps = state.list_apps(ctx).await?;
    let apps: Vec<AppJson> = apps.iter().map(to_json).collect();
    Ok(Json(serde_json::json!({ "apps": apps })).into_response())
}

async fn get_app(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let status = state.get_app(ctx, id).await?;
    Ok(Json(serde_json::json!({ "app": to_json(&status) })).into_response())
}

async fn app_disk(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let usage = state.app_disk(ctx, id).await?;
    Ok(Json(serde_json::json!({
        "app_dir_bytes": usage.app_dir_bytes,
        "quota_bytes": usage.quota_bytes,
        "image_bytes": usage.image_bytes,
        "repository_bytes": usage.repository_bytes,
        "data_bytes": usage.data_bytes,
        "volumes": usage.volumes.iter().map(|v| serde_json::json!({
            "entry": v.entry,
            "path": v.path,
            "bytes": v.bytes,
            "shared": v.shared,
            "counted": v.counted,
        })).collect::<Vec<_>>(),
    }))
    .into_response())
}

#[derive(Deserialize)]
struct InstallBody {
    /// "name", "stack" or "stack/app", optionally with "@version" — or a
    /// direct git repository URL (DMN-040).
    spec: String,
    /// Registry source to install from; required when several provide the package.
    #[serde(default)]
    source: Option<String>,
    /// Custom app name (DMN-024); for a stack — the per-app name prefix.
    #[serde(default)]
    name: Option<String>,
    /// Branch/tag to check out — direct repository installs only.
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    tag: Option<String>,
    /// Consent to the package license (DMN-028); without it a repository
    /// shipping a LICENSE fails with the structured license error.
    #[serde(default)]
    license_ack: bool,
}

async fn install_app(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Json(body): Json<InstallBody>,
) -> Result<Response, ApiError> {
    // Mirrors InstallAppResponse from the proto contract.
    let json = match state
        .install(
            ctx,
            body.spec,
            body.source,
            body.name,
            body.branch,
            body.tag,
            body.license_ack,
        )
        .await?
    {
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
    Extension(ctx): Extension<UserContext>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let outcome = state.start(ctx, id).await?;
    Ok(Json(serde_json::json!({
        "already_running": outcome == Outcome::AlreadyInState
    }))
    .into_response())
}

async fn stop_app(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let outcome = state.stop(ctx, id).await?;
    Ok(Json(serde_json::json!({
        "already_stopped": outcome == Outcome::AlreadyInState
    }))
    .into_response())
}

async fn restart_app(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    state.restart(ctx, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct LogsQuery {
    #[serde(default)]
    tail: Option<usize>,
}

async fn app_logs(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Path(id): Path<String>,
    Query(query): Query<LogsQuery>,
) -> Result<Response, ApiError> {
    let logs = state.logs(ctx, id, query.tail.unwrap_or(100)).await?;
    Ok(Json(serde_json::json!({ "logs": logs })).into_response())
}

async fn remove_app(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    state.remove(ctx, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct ConsoleTokenBody {
    /// "logs" or "attach" — mirrors ConsoleSessionType in the proto.
    session: String,
}

async fn console_token(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
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
    let (token, expires_at) = state.issue_console_token(ctx, id, session).await?;
    Ok(Json(serde_json::json!({ "token": token, "expires_at": expires_at })).into_response())
}
