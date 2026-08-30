//! gRPC transport: generated tonic services delegating to [`ApiState`].

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use tonic::{Request, Response, Status};

use super::console::SessionType;
use super::proto::v1 as pb;
use super::tokens;
use super::{ApiState, proto};
use crate::daemon::apps::{AppStatus, Outcome, RuntimeState, UserContext};
use crate::daemon::pkg::InstallOutcome;

use pb::app_service_server::{AppService, AppServiceServer};
use pb::daemon_service_server::{DaemonService, DaemonServiceServer};
use pb::monitor_service_server::{MonitorService, MonitorServiceServer};
use pb::system_service_server::{SystemService, SystemServiceServer};
use pb::token_service_server::{TokenService, TokenServiceServer};

/// gRPC routes as an axum router (mounted next to REST on one listener).
pub fn routes(state: Arc<ApiState>) -> Router {
    tonic::service::Routes::new(DaemonServiceServer::new(Grpc(Arc::clone(&state))))
        .add_service(AppServiceServer::new(Grpc(Arc::clone(&state))))
        .add_service(SystemServiceServer::new(Grpc(Arc::clone(&state))))
        .add_service(MonitorServiceServer::new(Grpc(Arc::clone(&state))))
        .add_service(TokenServiceServer::new(Grpc(state)))
        .into_axum_router()
}

struct Grpc(Arc<ApiState>);

/// The caller's context, stamped into the request extensions by the
/// transport middleware: the full-visibility context after bearer auth
/// (TCP), the peer-cred context on the unix socket. Absence means a
/// middleware bug — fail closed with an unprivileged nobody context
/// rather than defaulting to full visibility.
fn ctx_of<T>(request: &Request<T>) -> UserContext {
    request
        .extensions()
        .get::<UserContext>()
        .cloned()
        .unwrap_or(UserContext {
            uid: u32::MAX,
            name: "unauthenticated".into(),
            is_root: false,
        })
}

/// anyhow errors → gRPC status. "not found" errors keep their code; the rest
/// become INTERNAL with the message preserved.
fn to_status(err: anyhow::Error) -> Status {
    let msg = format!("{err:#}");
    if msg.contains("not found") || msg.contains("не найдено") {
        Status::not_found(msg)
    } else {
        Status::internal(msg)
    }
}

fn disk_to_pb(usage: &crate::daemon::apps::disk::DiskUsage) -> pb::GetAppDiskResponse {
    pb::GetAppDiskResponse {
        app_dir_bytes: usage.app_dir_bytes,
        quota_bytes: usage.quota_bytes,
        image_bytes: usage.image_bytes,
        repository_bytes: usage.repository_bytes,
        data_bytes: usage.data_bytes,
        volumes: usage
            .volumes
            .iter()
            .map(|v| pb::AppVolumeUsage {
                entry: v.entry.clone(),
                path: v.path.clone(),
                bytes: v.bytes,
                shared: v.shared,
                counted: v.counted,
            })
            .collect(),
    }
}

fn to_pb(status: &AppStatus) -> pb::App {
    pb::App {
        id: status.meta.id.clone(),
        uuid: status.meta.uuid.clone(),
        name: status.meta.display_name().to_string(),
        kind: status.meta.runtime.kind().to_string(),
        state: match status.state {
            RuntimeState::Running => pb::AppState::Running as i32,
            RuntimeState::Stopped => pb::AppState::Stopped as i32,
        },
        version: status.meta.version.clone().unwrap_or_default(),
        source: status.meta.source.clone().unwrap_or_default(),
        owner: status.meta.owner.name.clone(),
        title: status
            .meta
            .custom_name
            .is_some()
            .then(|| status.meta.name.clone()),
        quota: status.meta.quota.as_ref().map(|q| pb::AppQuota {
            cpu_cores: q.cpu_cores,
            ram_bytes: q.ram_bytes,
            disk_bytes: q.disk_bytes,
        }),
    }
}

#[tonic::async_trait]
impl DaemonService for Grpc {
    async fn get_status(
        &self,
        request: Request<pb::GetStatusRequest>,
    ) -> Result<Response<pb::GetStatusResponse>, Status> {
        let (running, total) = self.0.status(ctx_of(&request)).await.map_err(to_status)?;
        Ok(Response::new(pb::GetStatusResponse {
            version: crate::VERSION.to_string(),
            apps_total: total as u32,
            apps_running: running as u32,
        }))
    }
}

#[tonic::async_trait]
impl SystemService for Grpc {
    async fn reboot_system(
        &self,
        request: Request<pb::RebootSystemRequest>,
    ) -> Result<Response<pb::RebootSystemResponse>, Status> {
        let ctx = ctx_of(&request);
        tokens::require_primary(kind_of(&request), &ctx).map_err(denied_to_status)?;
        self.0.reboot_system().await.map_err(to_status)?;
        Ok(Response::new(pb::RebootSystemResponse { accepted: true }))
    }
}

fn metrics_to_pb(m: &crate::daemon::monitor::SystemMetrics) -> pb::SystemMetrics {
    pb::SystemMetrics {
        timestamp: m.timestamp,
        cpu_usage_percent: m.cpu.usage_percent,
        cpu_cores: m.cpu.cores,
        load1: m.cpu.load1,
        load5: m.cpu.load5,
        load15: m.cpu.load15,
        mem_total: m.memory.total,
        mem_used: m.memory.used,
        mem_available: m.memory.available,
        swap_total: m.memory.swap_total,
        swap_used: m.memory.swap_used,
        uptime_secs: m.uptime_secs,
        disks: m
            .disks
            .iter()
            .map(|d| pb::DiskMetrics {
                mount: d.mount.clone(),
                filesystem: d.filesystem.clone(),
                total: d.total,
                used: d.used,
                available: d.available,
            })
            .collect(),
        network: m
            .network
            .iter()
            .map(|n| pb::NetworkMetrics {
                interface: n.interface.clone(),
                rx_bytes: n.rx_bytes,
                tx_bytes: n.tx_bytes,
                rx_errors: n.rx_errors,
                tx_errors: n.tx_errors,
                rx_bytes_per_sec: n.rx_bytes_per_sec,
                tx_bytes_per_sec: n.tx_bytes_per_sec,
            })
            .collect(),
        gpus: m
            .gpus
            .iter()
            .map(|g| pb::GpuMetrics {
                index: g.index,
                vendor: g.vendor.clone(),
                name: g.name.clone(),
                utilization_percent: g.utilization_percent,
                memory_total: g.memory_total,
                memory_used: g.memory_used,
                temperature_c: g.temperature_c,
                power_watts: g.power_watts,
            })
            .collect(),
    }
}

#[tonic::async_trait]
impl MonitorService for Grpc {
    async fn get_system_metrics(
        &self,
        _request: Request<pb::GetSystemMetricsRequest>,
    ) -> Result<Response<pb::GetSystemMetricsResponse>, Status> {
        let metrics = self
            .0
            .monitor
            .latest()
            .ok_or_else(|| Status::unavailable("no metrics samples yet, retry shortly"))?;
        Ok(Response::new(pb::GetSystemMetricsResponse {
            metrics: Some(metrics_to_pb(&metrics)),
        }))
    }

    async fn get_metrics_history(
        &self,
        request: Request<pb::GetMetricsHistoryRequest>,
    ) -> Result<Response<pb::GetMetricsHistoryResponse>, Status> {
        let limit = request.into_inner().limit as usize;
        let samples = self.0.monitor.history(limit);
        Ok(Response::new(pb::GetMetricsHistoryResponse {
            samples: samples.iter().map(metrics_to_pb).collect(),
        }))
    }
}

#[tonic::async_trait]
impl AppService for Grpc {
    async fn list_apps(
        &self,
        request: Request<pb::ListAppsRequest>,
    ) -> Result<Response<pb::ListAppsResponse>, Status> {
        let apps = self
            .0
            .list_apps(ctx_of(&request))
            .await
            .map_err(to_status)?;
        Ok(Response::new(pb::ListAppsResponse {
            apps: apps.iter().map(to_pb).collect(),
        }))
    }

    async fn get_app(
        &self,
        request: Request<pb::GetAppRequest>,
    ) -> Result<Response<pb::GetAppResponse>, Status> {
        let ctx = ctx_of(&request);
        let status = self
            .0
            .get_app(ctx, request.into_inner().id)
            .await
            .map_err(to_status)?;
        Ok(Response::new(pb::GetAppResponse {
            app: Some(to_pb(&status)),
        }))
    }

    async fn get_app_disk(
        &self,
        request: Request<pb::GetAppDiskRequest>,
    ) -> Result<Response<pb::GetAppDiskResponse>, Status> {
        let ctx = ctx_of(&request);
        let (_, usage) = self
            .0
            .app_disk(ctx, request.into_inner().id)
            .await
            .map_err(to_status)?;
        Ok(Response::new(disk_to_pb(&usage)))
    }

    async fn install_app(
        &self,
        request: Request<pb::InstallAppRequest>,
    ) -> Result<Response<pb::InstallAppResponse>, Status> {
        let ctx = ctx_of(&request);
        let request = request.into_inner();
        let source = Some(request.source).filter(|s| !s.is_empty());
        let outcome = self
            .0
            .install(
                ctx,
                request.spec,
                source,
                request.name,
                request.branch,
                request.tag,
                request.license_ack,
                // The image-source choice (DMN-050) has no gRPC field yet; a
                // both-image manifest surfaces ImageChoiceRequired here. The
                // interactive flow is the CLI/REST path.
                None,
            )
            .await
            .map_err(to_status)?;
        let response = match outcome {
            InstallOutcome::App(report) => pb::InstallAppResponse {
                id: report.id,
                version: report.version,
                apps: vec![],
                skipped: vec![],
            },
            InstallOutcome::Stack {
                stack,
                installed,
                skipped,
            } => pb::InstallAppResponse {
                id: stack,
                version: installed
                    .first()
                    .map(|r| r.version.clone())
                    .unwrap_or_default(),
                apps: installed
                    .into_iter()
                    .map(|r| pb::InstalledApp {
                        id: r.id,
                        version: r.version,
                    })
                    .collect(),
                skipped,
            },
        };
        Ok(Response::new(response))
    }

    async fn start_app(
        &self,
        request: Request<pb::StartAppRequest>,
    ) -> Result<Response<pb::StartAppResponse>, Status> {
        let ctx = ctx_of(&request);
        let outcome = self
            .0
            .start(ctx, request.into_inner().id)
            .await
            .map_err(to_status)?;
        Ok(Response::new(pb::StartAppResponse {
            already_running: outcome == Outcome::AlreadyInState,
        }))
    }

    async fn stop_app(
        &self,
        request: Request<pb::StopAppRequest>,
    ) -> Result<Response<pb::StopAppResponse>, Status> {
        let ctx = ctx_of(&request);
        let outcome = self
            .0
            .stop(ctx, request.into_inner().id)
            .await
            .map_err(to_status)?;
        Ok(Response::new(pb::StopAppResponse {
            already_stopped: outcome == Outcome::AlreadyInState,
        }))
    }

    async fn restart_app(
        &self,
        request: Request<pb::RestartAppRequest>,
    ) -> Result<Response<pb::RestartAppResponse>, Status> {
        let ctx = ctx_of(&request);
        self.0
            .restart(ctx, request.into_inner().id)
            .await
            .map_err(to_status)?;
        Ok(Response::new(pb::RestartAppResponse {}))
    }

    async fn get_app_logs(
        &self,
        request: Request<pb::GetAppLogsRequest>,
    ) -> Result<Response<pb::GetAppLogsResponse>, Status> {
        let ctx = ctx_of(&request);
        let req = request.into_inner();
        let tail = if req.tail == 0 {
            100
        } else {
            req.tail as usize
        };
        let logs = self.0.logs(ctx, req.id, tail).await.map_err(to_status)?;
        Ok(Response::new(pb::GetAppLogsResponse { logs }))
    }

    async fn remove_app(
        &self,
        request: Request<pb::RemoveAppRequest>,
    ) -> Result<Response<pb::RemoveAppResponse>, Status> {
        let ctx = ctx_of(&request);
        self.0
            .remove(ctx, request.into_inner().id)
            .await
            .map_err(to_status)?;
        Ok(Response::new(pb::RemoveAppResponse {}))
    }

    async fn issue_console_token(
        &self,
        request: Request<pb::IssueConsoleTokenRequest>,
    ) -> Result<Response<pb::IssueConsoleTokenResponse>, Status> {
        let ctx = ctx_of(&request);
        let req = request.into_inner();
        let session = match proto::v1::ConsoleSessionType::try_from(req.session) {
            Ok(pb::ConsoleSessionType::Attach) => SessionType::Attach,
            Ok(pb::ConsoleSessionType::Logs) => SessionType::Logs,
            _ => return Err(Status::invalid_argument("session must be LOGS or ATTACH")),
        };
        let (token, expires_at) = self
            .0
            .issue_console_token(ctx, req.app_id, session)
            .await
            .map_err(to_status)?;
        Ok(Response::new(pb::IssueConsoleTokenResponse {
            token,
            expires_at,
        }))
    }
}

// ── API tokens (DMN-065, DMN-066 — see docs/security-tokens.md) ──

/// How the calling request authenticated, stamped by the transport
/// middleware next to the [`UserContext`]. Absent means a middleware bug, and
/// [`tokens::require_primary`] fails closed on `None` — the same posture
/// [`ctx_of`] takes.
fn kind_of<T>(request: &Request<T>) -> Option<tokens::Resolved> {
    request.extensions().get::<tokens::Resolved>().copied()
}

/// Token-management refusals become PERMISSION_DENIED, not INTERNAL: the
/// credential is valid, the operation simply is not open to it.
fn denied_to_status(err: tokens::TokenDenied) -> Status {
    Status::permission_denied(err.reason)
}

#[tonic::async_trait]
impl TokenService for Grpc {
    async fn issue_access_token(
        &self,
        request: Request<pb::IssueAccessTokenRequest>,
    ) -> Result<Response<pb::IssueAccessTokenResponse>, Status> {
        let ctx = ctx_of(&request);
        tokens::require_primary(kind_of(&request), &ctx).map_err(denied_to_status)?;
        let req = request.into_inner();
        let ttl = (req.ttl_secs > 0).then(|| Duration::from_secs(u64::from(req.ttl_secs)));
        let (token, expires_at) = self.0.tokens.issue_access(ttl, &req.label);
        Ok(Response::new(pb::IssueAccessTokenResponse {
            token,
            expires_at,
            ttl_secs: tokens::ACCESS_TTL.as_secs() as u32,
        }))
    }

    async fn revoke_access_tokens(
        &self,
        request: Request<pb::RevokeAccessTokensRequest>,
    ) -> Result<Response<pb::RevokeAccessTokensResponse>, Status> {
        let ctx = ctx_of(&request);
        tokens::require_primary(kind_of(&request), &ctx).map_err(denied_to_status)?;
        Ok(Response::new(pb::RevokeAccessTokensResponse {
            revoked: self.0.tokens.revoke_all_access() as u32,
        }))
    }

    async fn rotate_primary_token(
        &self,
        request: Request<pb::RotatePrimaryTokenRequest>,
    ) -> Result<Response<pb::RotatePrimaryTokenResponse>, Status> {
        let ctx = ctx_of(&request);
        tokens::require_primary(kind_of(&request), &ctx).map_err(denied_to_status)?;
        let grace = request
            .into_inner()
            .grace_secs
            .map_or(tokens::ROTATION_GRACE, |secs| {
                Duration::from_secs(u64::from(secs))
            });
        let path = super::api_token_path();
        let rotation = self
            .0
            .tokens
            .rotate(grace, |token| super::write_token(&path, token))
            .map_err(to_status)?;
        Ok(Response::new(pb::RotatePrimaryTokenResponse {
            token: rotation.token,
            rotated_at: rotation.rotated_at,
            grace_until: rotation.grace_until.unwrap_or_default(),
            revoked_access_tokens: rotation.revoked as u32,
        }))
    }

    async fn commit_primary_token_rotation(
        &self,
        request: Request<pb::CommitPrimaryTokenRotationRequest>,
    ) -> Result<Response<pb::CommitPrimaryTokenRotationResponse>, Status> {
        let ctx = ctx_of(&request);
        let resolved = kind_of(&request);
        tokens::require_primary(resolved, &ctx).map_err(denied_to_status)?;
        // Confirming with the token the rotation replaced would prove nothing
        // about whether the new one was stored.
        tokens::reject_grace(resolved).map_err(denied_to_status)?;
        self.0.tokens.commit_rotation();
        Ok(Response::new(pb::CommitPrimaryTokenRotationResponse {}))
    }

    async fn get_token_status(
        &self,
        request: Request<pb::GetTokenStatusRequest>,
    ) -> Result<Response<pb::GetTokenStatusResponse>, Status> {
        // Open to every kind of caller, so the response never carries token
        // material — the primary appears only as a truncated digest.
        let resolved = kind_of(&request).unwrap_or_else(tokens::Resolved::local_peer);
        let status = self.0.tokens.status(resolved);
        Ok(Response::new(pb::GetTokenStatusResponse {
            kind: match status.kind {
                tokens::TokenKind::Primary => pb::DaemonTokenKind::Primary,
                tokens::TokenKind::Access => pb::DaemonTokenKind::Access,
                tokens::TokenKind::LocalPeer => pb::DaemonTokenKind::Local,
            } as i32,
            expires_at: status.expires_at.unwrap_or_default(),
            access_tokens_live: status.access_tokens_live as u32,
            primary_digest: status.primary_digest,
            rotation_pending: status.rotation_pending,
            grace_until: status.grace_until.unwrap_or_default(),
            ttl_default_secs: status.ttl_default_secs as u32,
        }))
    }
}
