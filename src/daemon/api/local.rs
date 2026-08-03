//! Local-only REST endpoints used by the stdio MCP server.
//!
//! They are mounted exclusively by the Unix socket listener after its
//! SO_PEERCRED middleware. Keeping backup operations here avoids expanding
//! the public TCP/proto daemon API just for a local MCP transport.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::Deserialize;

use super::ApiState;
use crate::daemon::apps::UserContext;

pub fn router(state: Arc<ApiState>) -> Router {
    Router::new()
        .route(
            "/v1/local/apps/{id}/backups",
            get(list_backups).post(create_backup).delete(prune_backups),
        )
        .route("/v1/local/apps/{id}/backups/{name}", post(restore_backup))
        .with_state(state)
}

struct LocalError(anyhow::Error);

impl From<anyhow::Error> for LocalError {
    fn from(value: anyhow::Error) -> Self {
        Self(value)
    }
}

impl IntoResponse for LocalError {
    fn into_response(self) -> Response {
        let message = format!("{:#}", self.0);
        let code = if message.contains("not found") || message.contains("не найдено") {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        (code, Json(serde_json::json!({ "error": message }))).into_response()
    }
}

async fn list_backups(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Path(id): Path<String>,
) -> Result<Response, LocalError> {
    Ok(Json(serde_json::json!({ "backups": state.list_backups(ctx, id).await? })).into_response())
}

async fn create_backup(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Path(id): Path<String>,
) -> Result<Response, LocalError> {
    let info = state.create_backup(ctx, id).await?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "name": info.name,
            "storage": info.storage,
            "bytes": info.bytes,
        })),
    )
        .into_response())
}

async fn restore_backup(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Response, LocalError> {
    state.restore_backup(ctx, id, name).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct PruneBody {
    keep: u32,
}

async fn prune_backups(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Path(id): Path<String>,
    Json(body): Json<PruneBody>,
) -> Result<Response, LocalError> {
    let removed = state.prune_backups(ctx, id, body.keep).await?;
    Ok(Json(serde_json::json!({ "removed": removed })).into_response())
}
