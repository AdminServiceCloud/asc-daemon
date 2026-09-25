//! REST transport for backups and scheduled jobs (DMN-114/DMN-115) — the
//! same operations as BackupService/ScheduleService, field names mirroring
//! the proto messages (see docs/api.md).

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Extension, Json, Router};
use serde::Deserialize;

use super::ApiState;
use super::rest::ApiError;
use crate::daemon::apps::UserContext;
use crate::daemon::backup::storage::StorageKind;
use crate::daemon::scheduler::jobs;

pub fn routes() -> Router<Arc<ApiState>> {
    Router::new()
        .route("/v1/backups", get(list_backups))
        .route("/v1/backups/storages", get(list_storages))
        .route(
            "/v1/backups/storages/{name}",
            put(upsert_storage).delete(remove_storage),
        )
        .route("/v1/apps/{id}/backups", post(create_backup))
        .route("/v1/apps/{id}/backups/restore", post(restore_backup))
        .route(
            "/v1/apps/{id}/backups/{storage}/{name}",
            axum::routing::delete(delete_backup),
        )
        .route("/v1/schedules", get(list_schedules))
        .route("/v1/schedules/managed/{managed_by}", put(replace_managed))
        .route(
            "/v1/schedules/{id}",
            put(upsert_schedule).delete(remove_schedule),
        )
        .route("/v1/schedules/{id}/run", post(run_schedule))
        .route("/v1/schedules/{id}/runs", get(schedule_runs))
}

#[derive(Deserialize)]
struct ListBackupsQuery {
    app_id: Option<String>,
    storage: Option<String>,
}

async fn list_backups(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Query(query): Query<ListBackupsQuery>,
) -> Result<Response, ApiError> {
    let listing = state.backup_list(ctx, query.app_id, query.storage).await?;
    Ok(Json(listing).into_response())
}

async fn list_storages(State(state): State<Arc<ApiState>>) -> Result<Response, ApiError> {
    let storages = state.backup_storages().await?;
    Ok(Json(serde_json::json!({ "storages": storages })).into_response())
}

#[derive(Deserialize)]
struct S3Body {
    #[serde(default)]
    endpoint: String,
    #[serde(default)]
    region: String,
    bucket: String,
    access_key: String,
    secret_key: String,
    #[serde(default)]
    prefix: String,
}

#[derive(Deserialize)]
struct LocalBody {
    dir: String,
}

#[derive(Deserialize)]
struct UpsertStorageBody {
    managed_by: Option<String>,
    s3: Option<S3Body>,
    local: Option<LocalBody>,
}

async fn upsert_storage(
    State(state): State<Arc<ApiState>>,
    Path(name): Path<String>,
    Json(body): Json<UpsertStorageBody>,
) -> Result<Response, ApiError> {
    let kind = match (body.s3, body.local) {
        (Some(s3), None) => StorageKind::S3 {
            bucket: s3.bucket,
            region: s3.region,
            endpoint: Some(s3.endpoint).filter(|e| !e.is_empty()),
            access_key: s3.access_key,
            secret_key: s3.secret_key,
            prefix: Some(s3.prefix).filter(|p| !p.is_empty()),
        },
        (None, Some(local)) if !local.dir.trim().is_empty() => StorageKind::Local {
            dir: local.dir.into(),
        },
        _ => {
            return Ok(bad_request(
                "exactly one of s3 or local (with dir) is required",
            ));
        }
    };
    let storage = state
        .backup_storage_upsert(name, body.managed_by, kind)
        .await?;
    Ok(Json(serde_json::json!({ "storage": storage })).into_response())
}

#[derive(Deserialize)]
struct ManagedByQuery {
    managed_by: Option<String>,
}

async fn remove_storage(
    State(state): State<Arc<ApiState>>,
    Path(name): Path<String>,
    Query(query): Query<ManagedByQuery>,
) -> Result<Response, ApiError> {
    state.backup_storage_remove(name, query.managed_by).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct CreateBackupBody {
    #[serde(default)]
    storages: Vec<String>,
    keep: Option<u32>,
}

async fn create_backup(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Path(id): Path<String>,
    Json(body): Json<CreateBackupBody>,
) -> Result<Response, ApiError> {
    let results = state
        .backup_create(ctx, id, body.storages, body.keep)
        .await?;
    Ok(Json(serde_json::json!({ "results": results })).into_response())
}

#[derive(Deserialize)]
struct RestoreBody {
    storage: String,
    name: String,
    #[serde(default)]
    stop_app: bool,
}

async fn restore_backup(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Path(id): Path<String>,
    Json(body): Json<RestoreBody>,
) -> Result<Response, ApiError> {
    let restarted = state
        .backup_restore(ctx, id, body.storage, body.name, body.stop_app)
        .await?;
    Ok(Json(serde_json::json!({ "restarted": restarted })).into_response())
}

async fn delete_backup(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Path((id, storage, name)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    state.backup_delete(ctx, id, storage, name).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn list_schedules(
    State(state): State<Arc<ApiState>>,
    Query(query): Query<ManagedByQuery>,
) -> Result<Response, ApiError> {
    let schedules = state
        .schedules_list(query.managed_by.filter(|m| !m.is_empty()))
        .await?;
    Ok(Json(serde_json::json!({ "schedules": schedules })).into_response())
}

async fn upsert_schedule(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<String>,
    Json(mut job): Json<jobs::Job>,
) -> Result<Response, ApiError> {
    job.id = id;
    let schedule = state.schedule_upsert(job).await?;
    Ok(Json(serde_json::json!({ "schedule": schedule })).into_response())
}

async fn remove_schedule(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let removed = state.schedule_remove(id).await?;
    Ok(Json(serde_json::json!({ "removed": removed })).into_response())
}

#[derive(Deserialize)]
struct ReplaceManagedBody {
    #[serde(default)]
    schedules: Vec<jobs::Job>,
}

async fn replace_managed(
    State(state): State<Arc<ApiState>>,
    Path(managed_by): Path<String>,
    Json(body): Json<ReplaceManagedBody>,
) -> Result<Response, ApiError> {
    let schedules = state
        .schedules_replace_managed(managed_by, body.schedules)
        .await?;
    Ok(Json(serde_json::json!({ "schedules": schedules })).into_response())
}

async fn run_schedule(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let run = state.schedule_run(id).await?;
    Ok(Json(serde_json::json!({ "started": run.is_some(), "run": run })).into_response())
}

#[derive(Deserialize)]
struct RunsQuery {
    #[serde(default)]
    limit: usize,
}

async fn schedule_runs(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<String>,
    Query(query): Query<RunsQuery>,
) -> Result<Response, ApiError> {
    let runs = state.schedule_runs(id, query.limit).await?;
    Ok(Json(serde_json::json!({ "runs": runs })).into_response())
}

fn bad_request(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": message })),
    )
        .into_response()
}
