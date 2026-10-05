//! fail2ban API (DMN-150): the service layer over
//! [`crate::daemon::fail2ban::Fail2ban`] shared by both transports, the gRPC
//! `Fail2banService` and the REST routes the CLI uses. Every call is
//! root-only: a non-root unix-socket peer is refused before anything runs.

use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Extension, Json, Router};
use futures_util::stream::Stream;
use serde::Deserialize;
use tonic::{Request, Response as GrpcResponse, Status};

use super::ApiState;
use super::grpc::{Grpc, ctx_of, to_status};
use super::proto::v1 as pb;
use super::rest::ApiError;
use crate::daemon::apps::UserContext;
use crate::daemon::fail2ban::client::Ban;
use crate::daemon::fail2ban::model::{JailConfig, Settings};
use crate::daemon::fail2ban::{Fail2ban, Overview};
use crate::daemon::firewall::host::Facts;
use crate::daemon::users;

use pb::fail2ban_service_server::Fail2banService;

// ── Service layer ───────────────────────────────────────────────────────────

impl ApiState {
    async fn f2b<T, F>(self: &Arc<Self>, ctx: &UserContext, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Fail2ban, &Facts) -> Result<T> + Send + 'static,
    {
        users::require_root(ctx)?;
        let state = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let facts = state.host_facts();
            f(&state.fail2ban, &facts)
        })
        .await
        .context("fail2ban worker panicked")?
    }

    /// Installs fail2ban with progress lines on the channel; the last message
    /// is the result.
    fn f2b_install_stream(
        self: &Arc<Self>,
        ctx: UserContext,
    ) -> tokio::sync::mpsc::Receiver<InstallEvent> {
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let state = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            if let Err(err) = users::require_root(&ctx) {
                let _ = tx.blocking_send(InstallEvent::Done(Err(err.into())));
                return;
            }
            let mut progress = |line: &str| {
                let _ = tx.blocking_send(InstallEvent::Line(line.to_string()));
            };
            let facts = state.host_facts();
            let result = state.fail2ban.install(&facts, &mut progress).map(Box::new);
            let _ = tx.blocking_send(InstallEvent::Done(result));
        });
        rx
    }
}

enum InstallEvent {
    Line(String),
    Done(Result<Box<Overview>>),
}

// ── Proto conversions ───────────────────────────────────────────────────────

fn settings_to_pb(s: &Settings) -> pb::Fail2banSettings {
    pb::Fail2banSettings {
        bantime: s.bantime.clone(),
        findtime: s.findtime.clone(),
        maxretry: s.maxretry,
        bantime_increment: s.bantime_increment,
        ignoreip: s.ignoreip.clone(),
    }
}

fn settings_from_pb(p: pb::Fail2banSettings) -> Settings {
    Settings {
        bantime: p.bantime,
        findtime: p.findtime,
        maxretry: p.maxretry,
        bantime_increment: p.bantime_increment,
        ignoreip: p.ignoreip,
    }
}

fn jail_from_pb(p: pb::Fail2banJail) -> JailConfig {
    JailConfig {
        name: p.name,
        enabled: p.enabled,
        maxretry: (p.maxretry > 0).then_some(p.maxretry),
        bantime: Some(p.bantime),
        findtime: Some(p.findtime),
        port: Some(p.port),
        logpath: Some(p.logpath),
    }
}

fn state_to_pb(o: &Overview) -> pb::Fail2banState {
    pb::Fail2banState {
        installed: o.installed,
        running: o.running,
        version: o.version.clone(),
        settings: Some(settings_to_pb(&o.settings)),
        jails: o
            .jails
            .iter()
            .map(|j| {
                let status = j.status.clone().unwrap_or_default();
                pb::Fail2banJail {
                    name: j.config.name.clone(),
                    enabled: j.config.enabled,
                    maxretry: j.config.maxretry.unwrap_or(0),
                    bantime: j.config.bantime.clone().unwrap_or_default(),
                    findtime: j.config.findtime.clone().unwrap_or_default(),
                    port: j.config.port.clone().unwrap_or_default(),
                    logpath: j.config.logpath.clone().unwrap_or_default(),
                    available: j.available,
                    needs_web: j.needs_web,
                    active: j.active,
                    currently_failed: status.currently_failed,
                    total_failed: status.total_failed,
                    currently_banned: status.currently_banned,
                    total_banned: status.total_banned,
                }
            })
            .collect(),
        last_error: o.last_error.clone(),
    }
}

fn ban_to_pb(b: Ban) -> pb::Fail2banBan {
    pb::Fail2banBan {
        ip: b.ip,
        jail: b.jail,
        banned_unix: b.banned_unix,
        unban_unix: b.unban_unix,
    }
}

// ── gRPC ────────────────────────────────────────────────────────────────────

#[tonic::async_trait]
impl Fail2banService for Grpc {
    type InstallFail2banStreamStream =
        Pin<Box<dyn Stream<Item = Result<pb::Fail2banInstallEvent, Status>> + Send>>;

    async fn get_fail2ban(
        &self,
        request: Request<pb::GetFail2banRequest>,
    ) -> Result<GrpcResponse<pb::GetFail2banResponse>, Status> {
        let ctx = ctx_of(&request);
        let overview = self
            .0
            .f2b(&ctx, |f, facts| f.overview(facts))
            .await
            .map_err(to_status)?;
        Ok(GrpcResponse::new(pb::GetFail2banResponse {
            fail2ban: Some(state_to_pb(&overview)),
        }))
    }

    async fn install_fail2ban_stream(
        &self,
        request: Request<pb::InstallFail2banRequest>,
    ) -> Result<GrpcResponse<Self::InstallFail2banStreamStream>, Status> {
        let ctx = ctx_of(&request);
        let rx = self.0.f2b_install_stream(ctx);
        let stream = futures_util::stream::unfold(rx, |mut rx| async move {
            let event = match rx.recv().await? {
                InstallEvent::Line(line) => pb::fail2ban_install_event::Event::Log(line),
                InstallEvent::Done(Ok(overview)) => {
                    pb::fail2ban_install_event::Event::Done(state_to_pb(&overview))
                }
                InstallEvent::Done(Err(err)) => {
                    pb::fail2ban_install_event::Event::Error(format!("{err:#}"))
                }
            };
            Some((Ok(pb::Fail2banInstallEvent { event: Some(event) }), rx))
        });
        Ok(GrpcResponse::new(Box::pin(stream)))
    }

    async fn uninstall_fail2ban(
        &self,
        request: Request<pb::UninstallFail2banRequest>,
    ) -> Result<GrpcResponse<pb::UninstallFail2banResponse>, Status> {
        let ctx = ctx_of(&request);
        let purge = request.into_inner().purge;
        self.0
            .f2b(&ctx, move |f, _| f.uninstall(purge, &mut |_| {}))
            .await
            .map_err(to_status)?;
        Ok(GrpcResponse::new(pb::UninstallFail2banResponse {}))
    }

    async fn update_fail2ban_settings(
        &self,
        request: Request<pb::UpdateFail2banSettingsRequest>,
    ) -> Result<GrpcResponse<pb::UpdateFail2banSettingsResponse>, Status> {
        let ctx = ctx_of(&request);
        let settings = request
            .into_inner()
            .settings
            .ok_or_else(|| Status::invalid_argument("settings are required"))?;
        let settings = settings_from_pb(settings);
        let overview = self
            .0
            .f2b(&ctx, move |f, facts| f.update_settings(settings, facts))
            .await
            .map_err(to_status)?;
        Ok(GrpcResponse::new(pb::UpdateFail2banSettingsResponse {
            fail2ban: Some(state_to_pb(&overview)),
        }))
    }

    async fn upsert_fail2ban_jail(
        &self,
        request: Request<pb::UpsertFail2banJailRequest>,
    ) -> Result<GrpcResponse<pb::UpsertFail2banJailResponse>, Status> {
        let ctx = ctx_of(&request);
        let jail = request
            .into_inner()
            .jail
            .ok_or_else(|| Status::invalid_argument("jail is required"))?;
        let jail = jail_from_pb(jail);
        let overview = self
            .0
            .f2b(&ctx, move |f, facts| f.upsert_jail(jail, facts))
            .await
            .map_err(to_status)?;
        Ok(GrpcResponse::new(pb::UpsertFail2banJailResponse {
            fail2ban: Some(state_to_pb(&overview)),
        }))
    }

    async fn list_fail2ban_bans(
        &self,
        request: Request<pb::ListFail2banBansRequest>,
    ) -> Result<GrpcResponse<pb::ListFail2banBansResponse>, Status> {
        let ctx = ctx_of(&request);
        let jail = request.into_inner().jail;
        let bans = self
            .0
            .f2b(&ctx, move |f, _| {
                f.bans((!jail.is_empty()).then_some(jail.as_str()))
            })
            .await
            .map_err(to_status)?;
        Ok(GrpcResponse::new(pb::ListFail2banBansResponse {
            bans: bans.into_iter().map(ban_to_pb).collect(),
        }))
    }

    async fn ban_fail2ban_ip(
        &self,
        request: Request<pb::BanFail2banIpRequest>,
    ) -> Result<GrpcResponse<pb::BanFail2banIpResponse>, Status> {
        let ctx = ctx_of(&request);
        let req = request.into_inner();
        self.0
            .f2b(&ctx, move |f, _| f.ban(&req.jail, &req.ip))
            .await
            .map_err(to_status)?;
        Ok(GrpcResponse::new(pb::BanFail2banIpResponse {}))
    }

    async fn unban_fail2ban_ip(
        &self,
        request: Request<pb::UnbanFail2banIpRequest>,
    ) -> Result<GrpcResponse<pb::UnbanFail2banIpResponse>, Status> {
        let ctx = ctx_of(&request);
        let req = request.into_inner();
        let released = self
            .0
            .f2b(&ctx, move |f, _| f.unban(&req.jail, &req.ip))
            .await
            .map_err(to_status)?;
        Ok(GrpcResponse::new(pb::UnbanFail2banIpResponse { released }))
    }
}

// ── REST (CLI) ──────────────────────────────────────────────────────────────

pub fn routes() -> Router<Arc<ApiState>> {
    Router::new()
        .route("/v1/fail2ban", get(rest_get).delete(rest_uninstall))
        .route("/v1/fail2ban/install", post(rest_install))
        .route("/v1/fail2ban/settings", put(rest_settings))
        .route("/v1/fail2ban/jails/{name}", put(rest_jail))
        .route("/v1/fail2ban/bans", get(rest_bans).post(rest_ban))
        .route("/v1/fail2ban/unban", post(rest_unban))
}

async fn rest_get(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
) -> Result<Response, ApiError> {
    let overview = state.f2b(&ctx, |f, facts| f.overview(facts)).await?;
    Ok(Json(overview).into_response())
}

async fn rest_install(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
) -> Result<Response, ApiError> {
    let mut rx = state.f2b_install_stream(ctx);
    let mut log = Vec::new();
    while let Some(event) = rx.recv().await {
        match event {
            InstallEvent::Line(line) => log.push(line),
            InstallEvent::Done(result) => {
                let overview = result?;
                return Ok(
                    Json(serde_json::json!({ "log": log, "fail2ban": overview })).into_response(),
                );
            }
        }
    }
    Err(anyhow::anyhow!("the install worker stopped unexpectedly").into())
}

#[derive(Deserialize)]
struct UninstallQuery {
    #[serde(default)]
    purge: bool,
}

async fn rest_uninstall(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Query(query): Query<UninstallQuery>,
) -> Result<Response, ApiError> {
    let log = state
        .f2b(&ctx, move |f, _| {
            let mut log = Vec::new();
            f.uninstall(query.purge, &mut |l| log.push(l.to_string()))?;
            Ok(log)
        })
        .await?;
    Ok(Json(serde_json::json!({ "log": log })).into_response())
}

async fn rest_settings(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Json(settings): Json<Settings>,
) -> Result<Response, ApiError> {
    let overview = state
        .f2b(&ctx, move |f, facts| f.update_settings(settings, facts))
        .await?;
    Ok(Json(overview).into_response())
}

async fn rest_jail(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Path(name): Path<String>,
    Json(mut jail): Json<JailConfig>,
) -> Result<Response, ApiError> {
    jail.name = name;
    let overview = state
        .f2b(&ctx, move |f, facts| f.upsert_jail(jail, facts))
        .await?;
    Ok(Json(overview).into_response())
}

#[derive(Deserialize)]
struct BansQuery {
    jail: Option<String>,
}

async fn rest_bans(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Query(query): Query<BansQuery>,
) -> Result<Response, ApiError> {
    let bans = state
        .f2b(&ctx, move |f, _| {
            f.bans(query.jail.as_deref().filter(|j| !j.is_empty()))
        })
        .await?;
    Ok(Json(serde_json::json!({ "bans": bans })).into_response())
}

#[derive(Deserialize)]
struct BanBody {
    #[serde(default)]
    jail: String,
    ip: String,
}

async fn rest_ban(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Json(body): Json<BanBody>,
) -> Result<Response, ApiError> {
    state
        .f2b(&ctx, move |f, _| f.ban(&body.jail, &body.ip))
        .await?;
    Ok(Json(serde_json::json!({})).into_response())
}

async fn rest_unban(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Json(body): Json<BanBody>,
) -> Result<Response, ApiError> {
    let released = state
        .f2b(&ctx, move |f, _| f.unban(&body.jail, &body.ip))
        .await?;
    Ok(Json(serde_json::json!({ "released": released })).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jail_overrides_use_zero_and_empty_for_defaults() {
        let jail = jail_from_pb(pb::Fail2banJail {
            name: "sshd".into(),
            enabled: true,
            ..Default::default()
        });
        assert_eq!(jail.maxretry, None);
        // Empty strings are cleaned to "no override" by the model.
        let mut jail = jail;
        jail.normalize().unwrap();
        assert_eq!(jail.bantime, None);
        assert_eq!(jail.port, None);
        assert_eq!(jail.logpath, None);

        let jail = jail_from_pb(pb::Fail2banJail {
            name: "sshd".into(),
            maxretry: 3,
            bantime: "1d".into(),
            ..Default::default()
        });
        assert_eq!(jail.maxretry, Some(3));
        assert_eq!(jail.bantime.as_deref(), Some("1d"));
    }

    #[test]
    fn settings_round_trip_through_proto() {
        let settings = Settings {
            bantime: "2h".into(),
            findtime: "5m".into(),
            maxretry: 4,
            bantime_increment: false,
            ignoreip: vec!["10.0.0.0/8".into()],
        };
        assert_eq!(settings_from_pb(settings_to_pb(&settings)), settings);
    }
}
