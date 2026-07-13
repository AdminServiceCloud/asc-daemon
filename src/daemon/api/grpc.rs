//! gRPC transport: generated tonic services delegating to [`ApiState`].

use std::sync::Arc;

use axum::Router;
use tonic::{Request, Response, Status};

use super::console::SessionType;
use super::proto::v1 as pb;
use super::{ApiState, proto};
use crate::daemon::apps::{AppStatus, Outcome, RuntimeState};
use crate::daemon::pkg::InstallOutcome;

use pb::app_service_server::{AppService, AppServiceServer};
use pb::daemon_service_server::{DaemonService, DaemonServiceServer};
use pb::monitor_service_server::{MonitorService, MonitorServiceServer};

/// gRPC routes as an axum router (mounted next to REST on one listener).
pub fn routes(state: Arc<ApiState>) -> Router {
    tonic::service::Routes::new(DaemonServiceServer::new(Grpc(Arc::clone(&state))))
        .add_service(AppServiceServer::new(Grpc(Arc::clone(&state))))
        .add_service(MonitorServiceServer::new(Grpc(state)))
        .into_axum_router()
}

struct Grpc(Arc<ApiState>);

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

fn to_pb(status: &AppStatus) -> pb::App {
    pb::App {
        id: status.meta.id.clone(),
        name: status.meta.display_name().to_string(),
        kind: status.meta.runtime.kind().to_string(),
        state: match status.state {
            RuntimeState::Running => pb::AppState::Running as i32,
            RuntimeState::Stopped => pb::AppState::Stopped as i32,
        },
        version: status.meta.version.clone().unwrap_or_default(),
        source: status.meta.source.clone().unwrap_or_default(),
        owner: status.meta.owner.name.clone(),
    }
}

#[tonic::async_trait]
impl DaemonService for Grpc {
    async fn get_status(
        &self,
        _request: Request<pb::GetStatusRequest>,
    ) -> Result<Response<pb::GetStatusResponse>, Status> {
        let (running, total) = self.0.status().await.map_err(to_status)?;
        Ok(Response::new(pb::GetStatusResponse {
            version: crate::VERSION.to_string(),
            apps_total: total as u32,
            apps_running: running as u32,
        }))
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
        _request: Request<pb::ListAppsRequest>,
    ) -> Result<Response<pb::ListAppsResponse>, Status> {
        let apps = self.0.list_apps().await.map_err(to_status)?;
        Ok(Response::new(pb::ListAppsResponse {
            apps: apps.iter().map(to_pb).collect(),
        }))
    }

    async fn get_app(
        &self,
        request: Request<pb::GetAppRequest>,
    ) -> Result<Response<pb::GetAppResponse>, Status> {
        let status = self
            .0
            .get_app(request.into_inner().id)
            .await
            .map_err(to_status)?;
        Ok(Response::new(pb::GetAppResponse {
            app: Some(to_pb(&status)),
        }))
    }

    async fn install_app(
        &self,
        request: Request<pb::InstallAppRequest>,
    ) -> Result<Response<pb::InstallAppResponse>, Status> {
        let request = request.into_inner();
        let source = Some(request.source).filter(|s| !s.is_empty());
        let outcome = self
            .0
            .install(request.spec, source)
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
        let outcome = self
            .0
            .start(request.into_inner().id)
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
        let outcome = self
            .0
            .stop(request.into_inner().id)
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
        self.0
            .restart(request.into_inner().id)
            .await
            .map_err(to_status)?;
        Ok(Response::new(pb::RestartAppResponse {}))
    }

    async fn get_app_logs(
        &self,
        request: Request<pb::GetAppLogsRequest>,
    ) -> Result<Response<pb::GetAppLogsResponse>, Status> {
        let req = request.into_inner();
        let tail = if req.tail == 0 {
            100
        } else {
            req.tail as usize
        };
        let logs = self.0.logs(req.id, tail).await.map_err(to_status)?;
        Ok(Response::new(pb::GetAppLogsResponse { logs }))
    }

    async fn remove_app(
        &self,
        request: Request<pb::RemoveAppRequest>,
    ) -> Result<Response<pb::RemoveAppResponse>, Status> {
        self.0
            .remove(request.into_inner().id)
            .await
            .map_err(to_status)?;
        Ok(Response::new(pb::RemoveAppResponse {}))
    }

    async fn issue_console_token(
        &self,
        request: Request<pb::IssueConsoleTokenRequest>,
    ) -> Result<Response<pb::IssueConsoleTokenResponse>, Status> {
        let req = request.into_inner();
        let session = match proto::v1::ConsoleSessionType::try_from(req.session) {
            Ok(pb::ConsoleSessionType::Attach) => SessionType::Attach,
            Ok(pb::ConsoleSessionType::Logs) => SessionType::Logs,
            _ => return Err(Status::invalid_argument("session must be LOGS or ATTACH")),
        };
        let (token, expires_at) = self
            .0
            .issue_console_token(req.app_id, session)
            .await
            .map_err(to_status)?;
        Ok(Response::new(pb::IssueConsoleTokenResponse {
            token,
            expires_at,
        }))
    }
}
