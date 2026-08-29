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
use super::tokens;
use crate::daemon::apps::{AppStatus, Outcome, RuntimeState, UserContext};
use crate::daemon::pkg::InstallOutcome;

pub fn router(state: Arc<ApiState>) -> Router {
    Router::new()
        .route("/v1/status", get(status))
        .route("/v1/metrics", get(system_metrics))
        .route("/v1/metrics/history", get(metrics_history))
        .route("/v1/apps", get(list_apps).post(install_app))
        // Apps-wide reports (DMN-053): the per-app routes below answer for
        // one app, these for every app the caller may see — the figures the
        // CLI cannot compute itself without reading the app tree.
        .route("/v1/disk", get(disk_summary))
        .route("/v1/ports", get(ports_summary))
        .route("/v1/stats", get(stats))
        .route("/v1/apps/{id}", get(get_app).delete(remove_app))
        .route("/v1/apps/{id}/disk", get(app_disk))
        .route("/v1/apps/{id}/ports", get(app_ports))
        .route("/v1/apps/{id}/upgrade", post(upgrade_app))
        .route("/v1/apps/{id}/start", post(start_app))
        .route("/v1/apps/{id}/stop", post(stop_app))
        .route("/v1/apps/{id}/restart", post(restart_app))
        .route("/v1/apps/{id}/logs", get(app_logs))
        .route(
            "/v1/apps/{id}/settings",
            get(app_settings).put(set_app_settings),
        )
        .route("/v1/apps/{id}/console-token", post(console_token))
        // API tokens (DMN-065/DMN-066). Mounted here, so the CLI reaches them
        // over the unix socket too — `asc api token …` needs no bearer.
        .route("/v1/token", get(token_status))
        .route(
            "/v1/token/access",
            post(issue_access_token).delete(revoke_access_tokens),
        )
        .route("/v1/token/rotate", post(rotate_primary_token))
        .route("/v1/token/rotate/commit", post(commit_token_rotation))
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

impl From<tokens::TokenDenied> for ApiError {
    fn from(err: tokens::TokenDenied) -> Self {
        Self(err.into())
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
        if let Some(choice) = self
            .0
            .downcast_ref::<crate::daemon::pkg::VersionChoiceRequired>()
        {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": msg,
                    "version_choice": {
                        "package": choice.package,
                        "source": choice.source,
                        "tags": choice.tags,
                        "branches": choice.branches,
                    },
                })),
            )
                .into_response();
        }
        if let Some(choice) = self
            .0
            .downcast_ref::<crate::daemon::pkg::ImageChoiceRequired>()
        {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": msg,
                    "image_choice": {
                        "package": choice.package,
                        "image": choice.image,
                        "build": choice.build,
                    },
                })),
            )
                .into_response();
        }
        // Token management attempted with something other than the primary
        // (DMN-065). 403, not 401: the credential is valid, the operation is
        // not open to it, and a client must not read this as "refresh me".
        if let Some(denied) = self.0.downcast_ref::<super::tokens::TokenDenied>() {
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "error": msg,
                    "token_denied": { "reason": denied.reason },
                })),
            )
                .into_response();
        }
        // The repository is private and nothing the caller configured opens
        // it (DMN-062). The URL travels structured so the CLI can offer the
        // token / ssh-key setup right there and retry, instead of leaving the
        // user to read an `asc auth add` hint out of a message.
        if let Some(required) = self
            .0
            .downcast_ref::<crate::daemon::pkg::auth::AuthRequired>()
        {
            // 409, not 401: on the TCP listener a 401 is what a bad bearer
            // token answers with, and the platform must not read "this
            // repository is private" as "my API token expired".
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": msg,
                    "auth_required": { "url": required.url },
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
    /// Registry install spec (`name` or `stack/app`) — what groups an app
    /// under its stack in `asc stacks` (DMN-051); absent for apps installed
    /// straight from a repository URL and for pre-DMN-003 installs.
    #[serde(skip_serializing_if = "Option::is_none")]
    package: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    quota: Option<crate::daemon::apps::meta::Quota>,
    /// Stable instance identity (DMN-044); absent for pre-DMN-044 installs.
    #[serde(skip_serializing_if = "Option::is_none")]
    uuid: Option<String>,
}

fn to_json(status: &AppStatus) -> AppJson {
    AppJson {
        id: status.meta.id.clone(),
        uuid: status.meta.uuid.clone(),
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
        package: status.meta.package.clone(),
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
        "gpus": m.gpus.iter().map(|g| serde_json::json!({
            "index": g.index,
            "vendor": g.vendor,
            "name": g.name,
            "utilization_percent": g.utilization_percent,
            "memory_total": g.memory_total,
            "memory_used": g.memory_used,
            "temperature_c": g.temperature_c,
            "power_watts": g.power_watts,
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
    let (meta, usage) = state.app_disk(ctx, id).await?;
    Ok(Json(serde_json::json!({
        // The resolved app: `id` in the path may have been a custom name.
        "id": meta.id,
        "name": meta.display_name(),
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

/// `asc disk` with no app: every visible app's footprint plus the capacity of
/// the filesystem the app store lives on (DMN-053).
async fn disk_summary(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
) -> Result<Response, ApiError> {
    let summary = state.disk_summary(ctx).await?;
    Ok(Json(serde_json::json!({
        "fs_total": summary.fs_total,
        "apps": summary.apps.iter().map(|app| serde_json::json!({
            "id": app.id,
            "name": app.name,
            "owner": app.owner,
            "bytes": app.bytes,
        })).collect::<Vec<_>>(),
    }))
    .into_response())
}

async fn app_ports(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let (meta, ports) = state.app_ports(ctx, id).await?;
    Ok(Json(serde_json::json!({
        "id": meta.id,
        "name": meta.display_name(),
        "ports": ports,
    }))
    .into_response())
}

async fn ports_summary(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
) -> Result<Response, ApiError> {
    let rows = state.ports_summary(ctx).await?;
    Ok(Json(serde_json::json!({
        "apps": rows.iter().map(|app| serde_json::json!({
            "id": app.id,
            "name": app.name,
            "owner": app.owner,
            "ports": app.ports,
        })).collect::<Vec<_>>(),
    }))
    .into_response())
}

/// Resource consumption per app, like `docker stats --no-stream`. Costs the
/// sampling interval (~500 ms) per call — the CPU percentage is a delta of
/// two readings.
async fn stats(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
) -> Result<Response, ApiError> {
    let stats = state.stats(ctx).await?;
    Ok(Json(serde_json::json!({
        "apps": stats.iter().map(|s| serde_json::json!({
            "id": s.meta.id,
            "name": s.meta.display_name(),
            "kind": s.meta.runtime.kind(),
            "owner": s.meta.owner.name,
            "cpu_percent": s.cpu_percent,
            "memory_bytes": s.memory_bytes,
            "disk_bytes": s.disk_bytes,
            "quota_disk_bytes": s.quota_disk_bytes,
            "disk_read_bytes": s.disk_read_bytes,
            "disk_write_bytes": s.disk_write_bytes,
            "net_rx_bytes": s.net_rx_bytes,
            "net_tx_bytes": s.net_tx_bytes,
            "disk_read_rate": s.disk_read_rate,
            "disk_write_rate": s.disk_write_rate,
            "net_rx_rate": s.net_rx_rate,
            "net_tx_rate": s.net_tx_rate,
        })).collect::<Vec<_>>(),
    }))
    .into_response())
}

#[derive(Deserialize)]
struct UpgradeBody {
    /// Target version (a git tag of the package repository); absent — the
    /// newest tag, or the tracked branch for a direct repository install.
    #[serde(default)]
    version: Option<String>,
}

/// Upgrade one app (DMN-053). The app must be stopped; the caller must own
/// it. Cloning uses the daemon's git credentials.
async fn upgrade_app(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Path(id): Path<String>,
    body: Option<Json<UpgradeBody>>,
) -> Result<Response, ApiError> {
    let spec = match body.and_then(|Json(body)| body.version) {
        Some(version) => format!("{id}@{version}"),
        None => id,
    };
    let json = match state.upgrade(ctx, spec).await? {
        // The commits are full shas (DMN-056); abbreviating them is the
        // caller's choice.
        crate::daemon::pkg::UpgradeOutcome::Upgraded {
            id,
            from,
            to,
            from_commit,
            to_commit,
        } => serde_json::json!({
            "id": id,
            "up_to_date": false,
            "from": from,
            "to": to,
            "from_commit": from_commit,
            "to_commit": to_commit,
        }),
        crate::daemon::pkg::UpgradeOutcome::UpToDate { id, version } => serde_json::json!({
            "id": id,
            "up_to_date": true,
            "version": version,
        }),
    };
    Ok(Json(json).into_response())
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
    /// Image source when the manifest offers both `image` and `image-build`
    /// (DMN-050): "prebuilt" or "build". Absent → the structured image-choice
    /// error for such manifests.
    #[serde(default)]
    image_choice: Option<crate::daemon::apps::ImageSource>,
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
            body.image_choice,
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

/// The app's settings schema (`asc.settings.yaml`, `null` when the package
/// defines none) together with the values chosen so far — everything an
/// out-of-process editor needs (DMN-043).
async fn app_settings(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let (file, values) = state.app_settings(ctx, id).await?;
    Ok(Json(serde_json::json!({
        "settings": file,
        "values": values.as_map(),
    }))
    .into_response())
}

#[derive(Deserialize)]
struct SettingsBody {
    /// The complete value map, as the editor leaves it: what is absent here
    /// is reset to the package default.
    values: serde_json::Map<String, serde_json::Value>,
}

async fn set_app_settings(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Path(id): Path<String>,
    Json(body): Json<SettingsBody>,
) -> Result<Response, ApiError> {
    let values = crate::daemon::pkg::settings::SettingValues::from_map(body.values);
    state.set_app_settings(ctx, id, values).await?;
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

// ── API tokens (DMN-065, DMN-066 — see docs/security-tokens.md) ──
//
// Four routes, three of them behind `require_primary`: an access token may
// drive the whole daemon but may not touch the tokens themselves.

#[derive(Deserialize, Default)]
struct IssueAccessBody {
    /// Requested lifetime; clamped by the store. Omitted → the 10-minute
    /// default.
    ttl_secs: Option<u64>,
    /// Free-form note about who asked, surfaced by `asc api status`.
    #[serde(default)]
    label: String,
}

async fn issue_access_token(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    resolved: Option<Extension<tokens::Resolved>>,
    body: Option<Json<IssueAccessBody>>,
) -> Result<Response, ApiError> {
    tokens::require_primary(resolved.map(|Extension(r)| r), &ctx)?;
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let ttl = body.ttl_secs.map(std::time::Duration::from_secs);
    let (token, expires_at) = state.tokens.issue_access(ttl, &body.label);
    Ok(Json(serde_json::json!({
        "token": token,
        "expires_at": expires_at,
        "ttl_secs": tokens::ACCESS_TTL.as_secs(),
    }))
    .into_response())
}

async fn revoke_access_tokens(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    resolved: Option<Extension<tokens::Resolved>>,
) -> Result<Response, ApiError> {
    tokens::require_primary(resolved.map(|Extension(r)| r), &ctx)?;
    let revoked = state.tokens.revoke_all_access();
    Ok(Json(serde_json::json!({ "revoked": revoked })).into_response())
}

#[derive(Deserialize, Default)]
struct RotateBody {
    /// How long the previous primary keeps working. Omitted → 300s, `0` →
    /// switch over immediately.
    grace_secs: Option<u64>,
}

async fn rotate_primary_token(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    resolved: Option<Extension<tokens::Resolved>>,
    body: Option<Json<RotateBody>>,
) -> Result<Response, ApiError> {
    tokens::require_primary(resolved.map(|Extension(r)| r), &ctx)?;
    let grace = body
        .map(|Json(b)| b)
        .unwrap_or_default()
        .grace_secs
        .map_or(tokens::ROTATION_GRACE, std::time::Duration::from_secs);
    let path = super::api_token_path();
    let rotation = state
        .tokens
        .rotate(grace, |token| super::write_token(&path, token))?;
    Ok(Json(serde_json::json!({
        "token": rotation.token,
        "rotated_at": rotation.rotated_at,
        "grace_until": rotation.grace_until,
        "revoked_access_tokens": rotation.revoked,
    }))
    .into_response())
}

async fn commit_token_rotation(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    resolved: Option<Extension<tokens::Resolved>>,
) -> Result<Response, ApiError> {
    let resolved = resolved.map(|Extension(r)| r);
    tokens::require_primary(resolved, &ctx)?;
    // The whole point of the confirmation is that the caller can already use
    // the new token; confirming with the one it replaced proves nothing.
    tokens::reject_grace(resolved)?;
    state.tokens.commit_rotation();
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn token_status(
    State(state): State<Arc<ApiState>>,
    resolved: Option<Extension<tokens::Resolved>>,
) -> Result<Response, ApiError> {
    // Open to every kind of caller, so it must never carry a secret — the
    // primary appears only as a truncated digest.
    let resolved = resolved
        .map(|Extension(r)| r)
        .unwrap_or_else(tokens::Resolved::local_peer);
    let status = state.tokens.status(resolved);
    Ok(Json(serde_json::json!({
        "kind": match status.kind {
            tokens::TokenKind::Primary => "primary",
            tokens::TokenKind::Access => "access",
            tokens::TokenKind::LocalPeer => "local",
        },
        "expires_at": status.expires_at,
        "access_tokens_live": status.access_tokens_live,
        "primary_digest": status.primary_digest,
        "rotation": {
            "pending": status.rotation_pending,
            "grace_until": status.grace_until,
        },
        "ttl_default_secs": status.ttl_default_secs,
    }))
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    /// The producing side of the DMN-062 contract the client reconstructs in
    /// `daemon::client::typed_error`: a private repository must reach the
    /// caller as a structured `auth_required`, not as a message to parse.
    #[tokio::test]
    async fn private_repository_is_reported_structurally() {
        let err = ApiError(anyhow::Error::new(crate::daemon::pkg::auth::AuthRequired {
            url: "https://github.com/org/private".into(),
        }));
        let response = err.into_response();
        // Not 401: on the TCP listener that status belongs to the bearer
        // token, and this is about the repository, not about the API.
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json["auth_required"]["url"],
            "https://github.com/org/private"
        );
        assert!(json["error"].as_str().is_some_and(|m| !m.is_empty()));
    }
}
