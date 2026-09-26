//! Web server API (DMN-122..DMN-125): the service layer over
//! [`crate::daemon::webserver::WebServer`] shared by both transports, the
//! gRPC `WebServerService` and the REST routes the CLI uses. Every call is
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
use crate::daemon::users;
use crate::daemon::webserver::model::{
    Balance, Header, HealthCheck, HealthKind, Mode, Proxy, RealIp, Settings, Site, SiteState,
    SiteView, Target, Tls, TlsMode, TlsState, Upstream, UpstreamServer,
};
use crate::daemon::webserver::{Overview, WebServer};

use pb::web_server_service_server::WebServerService;

// ── Service layer ───────────────────────────────────────────────────────────

impl ApiState {
    async fn web<T, F>(self: &Arc<Self>, ctx: &UserContext, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&WebServer) -> Result<T> + Send + 'static,
    {
        users::require_root(ctx)?;
        let web = Arc::clone(&self.webserver);
        tokio::task::spawn_blocking(move || f(&web))
            .await
            .context("web server worker panicked")?
    }

    /// Install with progress lines on `tx`; the final message is the result.
    fn web_install_stream(
        self: &Arc<Self>,
        ctx: UserContext,
        mode: Mode,
    ) -> tokio::sync::mpsc::Receiver<InstallEvent> {
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let web = Arc::clone(&self.webserver);
        tokio::task::spawn_blocking(move || {
            if let Err(err) = users::require_root(&ctx) {
                let _ = tx.blocking_send(InstallEvent::Done(Err(err.into())));
                return;
            }
            let mut progress = |line: &str| {
                let _ = tx.blocking_send(InstallEvent::Line(line.to_string()));
            };
            let result = web.install(mode, &mut progress).map(Box::new);
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

fn mode_to_pb(mode: Mode) -> i32 {
    let value = match mode {
        Mode::None => pb::WebServerMode::Unspecified,
        Mode::System => pb::WebServerMode::System,
        Mode::Docker => pb::WebServerMode::Docker,
    };
    value as i32
}

fn mode_from_pb(value: i32) -> Result<Mode, Status> {
    match pb::WebServerMode::try_from(value) {
        Ok(pb::WebServerMode::System) => Ok(Mode::System),
        Ok(pb::WebServerMode::Docker) => Ok(Mode::Docker),
        _ => Err(Status::invalid_argument("mode must be SYSTEM or DOCKER")),
    }
}

fn settings_to_pb(s: &Settings) -> pb::WebServerSettings {
    pb::WebServerSettings {
        worker_processes: s.worker_processes.clone(),
        worker_connections: s.worker_connections,
        keepalive_timeout_seconds: s.keepalive_timeout,
        client_max_body_size: s.client_max_body_size.clone(),
        gzip: s.gzip,
        gzip_level: s.gzip_level,
        server_tokens: s.server_tokens,
        http2: s.http2,
        tls_protocols: s.tls_protocols.clone(),
        hsts: s.hsts,
        cloudflare_real_ip: s.cloudflare_real_ip,
        access_log: s.access_log,
        acme_email: s.acme_email.clone(),
        acme_directory: s.acme_directory.clone(),
        custom_main: s.custom_main.clone(),
        custom_http: s.custom_http.clone(),
        image: s.image.clone(),
    }
}

fn settings_from_pb(p: pb::WebServerSettings) -> Settings {
    Settings {
        worker_processes: p.worker_processes,
        worker_connections: p.worker_connections,
        keepalive_timeout: p.keepalive_timeout_seconds,
        client_max_body_size: p.client_max_body_size,
        gzip: p.gzip,
        gzip_level: p.gzip_level,
        server_tokens: p.server_tokens,
        http2: p.http2,
        tls_protocols: p.tls_protocols,
        hsts: p.hsts,
        cloudflare_real_ip: p.cloudflare_real_ip,
        access_log: p.access_log,
        acme_email: p.acme_email,
        acme_directory: p.acme_directory,
        custom_main: p.custom_main,
        custom_http: p.custom_http,
        image: p.image,
        ..Settings::default()
    }
}

fn overview_to_pb(o: &Overview) -> pb::GetWebServerResponse {
    pb::GetWebServerResponse {
        installed: o.installed,
        mode: mode_to_pb(o.mode),
        engine: o.engine.to_string(),
        version: o.version.clone().unwrap_or_default(),
        running: o.running,
        adopted: o.adopted,
        settings: Some(settings_to_pb(&o.settings)),
        last_error: o.last_error.clone(),
        last_applied_unix: o.last_applied,
        cloudflare_updated_unix: o.cloudflare_updated,
        site_count: o.site_count as u32,
        nginx_present: o.nginx_present,
        docker_available: o.docker_available,
    }
}

fn balance_to_pb(b: Balance) -> i32 {
    let value = match b {
        Balance::RoundRobin => pb::SiteBalance::RoundRobin,
        Balance::LeastConn => pb::SiteBalance::LeastConn,
        Balance::IpHash => pb::SiteBalance::IpHash,
        Balance::Hash => pb::SiteBalance::Hash,
    };
    value as i32
}

fn balance_from_pb(v: i32) -> Balance {
    match pb::SiteBalance::try_from(v) {
        Ok(pb::SiteBalance::LeastConn) => Balance::LeastConn,
        Ok(pb::SiteBalance::IpHash) => Balance::IpHash,
        Ok(pb::SiteBalance::Hash) => Balance::Hash,
        _ => Balance::RoundRobin,
    }
}

fn tls_mode_to_pb(m: TlsMode) -> i32 {
    let value = match m {
        TlsMode::None => pb::SiteTlsMode::None,
        TlsMode::Acme => pb::SiteTlsMode::Acme,
        TlsMode::Provided => pb::SiteTlsMode::Provided,
    };
    value as i32
}

fn tls_mode_from_pb(v: i32) -> TlsMode {
    match pb::SiteTlsMode::try_from(v) {
        Ok(pb::SiteTlsMode::Acme) => TlsMode::Acme,
        Ok(pb::SiteTlsMode::Provided) => TlsMode::Provided,
        _ => TlsMode::None,
    }
}

fn headers_to_pb(h: &[Header]) -> Vec<pb::SiteHeader> {
    h.iter()
        .map(|h| pb::SiteHeader {
            name: h.name.clone(),
            value: h.value.clone(),
        })
        .collect()
}

fn headers_from_pb(h: Vec<pb::SiteHeader>) -> Vec<Header> {
    h.into_iter()
        .map(|h| Header {
            name: h.name,
            value: h.value,
        })
        .collect()
}

fn site_state_to_pb(s: SiteState) -> i32 {
    let value = match s {
        SiteState::Pending => pb::SiteState::Pending,
        SiteState::Applied => pb::SiteState::Applied,
        SiteState::Error => pb::SiteState::Error,
        SiteState::Disabled => pb::SiteState::Disabled,
    };
    value as i32
}

fn tls_state_to_pb(s: TlsState) -> i32 {
    let value = match s {
        TlsState::None => pb::SiteTlsState::None,
        TlsState::PendingDns => pb::SiteTlsState::PendingDns,
        TlsState::Issuing => pb::SiteTlsState::Issuing,
        TlsState::Active => pb::SiteTlsState::Active,
        TlsState::Expiring => pb::SiteTlsState::Expiring,
        TlsState::Error => pb::SiteTlsState::Error,
    };
    value as i32
}

fn view_to_pb(view: SiteView) -> pb::Site {
    let SiteView { site, status } = view;
    let mut out = site_to_pb(site);
    out.status = Some(pb::SiteStatus {
        state: site_state_to_pb(status.state),
        message: status.message,
        applied_unix: status.applied_at,
        tls: Some(pb::SiteTlsStatus {
            state: tls_state_to_pb(status.tls.state),
            not_after_unix: status.tls.not_after,
            issuer: status.tls.issuer,
            last_error: status.tls.last_error,
            next_attempt_unix: status.tls.next_attempt,
        }),
        upstream_addresses: status.upstream_addresses,
        upstream_health: status
            .upstream_health
            .into_iter()
            .map(|h| pb::SiteUpstreamHealth {
                address: h.address,
                healthy: h.healthy,
                checked: h.checked,
                last_error: h.last_error,
                checked_unix: h.checked_at,
                latency_ms: h.latency_ms,
            })
            .collect(),
    });
    out
}

fn health_to_pb(check: HealthCheck) -> pb::SiteHealthCheck {
    let kind = match check.kind {
        HealthKind::Off => pb::SiteHealthCheckType::Off,
        HealthKind::Tcp => pb::SiteHealthCheckType::Tcp,
        HealthKind::Http => pb::SiteHealthCheckType::Http,
    };
    pb::SiteHealthCheck {
        r#type: kind as i32,
        path: check.path,
        expected_status: u32::from(check.expected_status),
        interval_seconds: check.interval_secs,
        timeout_seconds: check.timeout_secs,
        fails: check.fails,
        passes: check.passes,
    }
}

fn health_from_pb(p: Option<pb::SiteHealthCheck>) -> HealthCheck {
    let Some(p) = p else {
        return HealthCheck::default();
    };
    let kind = match pb::SiteHealthCheckType::try_from(p.r#type) {
        Ok(pb::SiteHealthCheckType::Tcp) => HealthKind::Tcp,
        Ok(pb::SiteHealthCheckType::Http) => HealthKind::Http,
        _ => HealthKind::Off,
    };
    HealthCheck {
        kind,
        path: p.path,
        expected_status: u16::try_from(p.expected_status).unwrap_or(0),
        interval_secs: p.interval_seconds,
        timeout_secs: p.timeout_seconds,
        fails: p.fails,
        passes: p.passes,
    }
}

fn site_to_pb(site: Site) -> pb::Site {
    pb::Site {
        id: site.id,
        server_names: site.server_names,
        managed_by: site.managed_by,
        disabled: site.disabled,
        upstream: Some(pb::SiteUpstream {
            servers: site
                .upstream
                .servers
                .into_iter()
                .map(|s| pb::SiteUpstreamServer {
                    target: Some(match s.target {
                        Target::App { app, port } => {
                            pb::site_upstream_server::Target::App(pb::SiteUpstreamApp {
                                app,
                                port: u32::from(port),
                            })
                        }
                        Target::Address { address } => {
                            pb::site_upstream_server::Target::Address(address)
                        }
                    }),
                    weight: s.weight,
                    backup: s.backup,
                    max_fails: s.max_fails,
                    fail_timeout_seconds: s.fail_timeout_secs,
                    down: s.down,
                })
                .collect(),
            balance: balance_to_pb(site.upstream.balance),
            hash_key: site.upstream.hash_key,
            keepalive: site.upstream.keepalive,
            tls: site.upstream.tls,
            health_check: Some(health_to_pb(site.upstream.health_check)),
        }),
        tls: Some(pb::SiteTls {
            mode: tls_mode_to_pb(site.tls.mode),
            certificate_pem: String::new(),
            private_key_pem: String::new(),
            redirect_http: site.tls.redirect_http,
            hsts: site.tls.hsts,
            http2: site.tls.http2,
        }),
        proxy: Some(pb::SiteProxy {
            websocket: site.proxy.websocket,
            client_max_body_size: site.proxy.client_max_body_size,
            connect_timeout_seconds: site.proxy.connect_timeout_secs,
            read_timeout_seconds: site.proxy.read_timeout_secs,
            send_timeout_seconds: site.proxy.send_timeout_secs,
            request_headers: headers_to_pb(&site.proxy.request_headers),
            response_headers: headers_to_pb(&site.proxy.response_headers),
            upstream_host: site.proxy.upstream_host,
            no_buffering: site.proxy.no_buffering,
        }),
        real_ip: if site.real_ip == RealIp::Cloudflare {
            pb::SiteRealIp::Cloudflare as i32
        } else {
            pb::SiteRealIp::Off as i32
        },
        extra_server: site.extra_server,
        extra_location: site.extra_location,
        raw_config: site.raw_config,
        status: None,
    }
}

fn site_from_pb(p: pb::Site) -> Result<Site, Status> {
    let upstream = p.upstream.unwrap_or_default();
    let mut servers = Vec::with_capacity(upstream.servers.len());
    for s in upstream.servers {
        let target = match s.target {
            Some(pb::site_upstream_server::Target::App(app)) => Target::App {
                app: app.app,
                port: u16::try_from(app.port)
                    .map_err(|_| Status::invalid_argument("upstream port out of range"))?,
            },
            Some(pb::site_upstream_server::Target::Address(address)) => Target::Address { address },
            None => return Err(Status::invalid_argument("upstream server needs a target")),
        };
        servers.push(UpstreamServer {
            target,
            weight: s.weight.max(1),
            backup: s.backup,
            max_fails: s.max_fails,
            fail_timeout_secs: s.fail_timeout_seconds,
            down: s.down,
        });
    }
    let tls = p.tls.unwrap_or_default();
    let proxy = p.proxy.unwrap_or_default();
    Ok(Site {
        id: p.id,
        server_names: p.server_names,
        managed_by: p.managed_by.filter(|m| !m.is_empty()),
        disabled: p.disabled,
        upstream: Upstream {
            servers,
            balance: balance_from_pb(upstream.balance),
            hash_key: upstream.hash_key,
            keepalive: upstream.keepalive,
            tls: upstream.tls,
            health_check: health_from_pb(upstream.health_check),
        },
        tls: Tls {
            mode: tls_mode_from_pb(tls.mode),
            certificate_pem: tls.certificate_pem,
            private_key_pem: tls.private_key_pem,
            redirect_http: tls.redirect_http,
            hsts: tls.hsts,
            http2: tls.http2,
        },
        proxy: Proxy {
            websocket: proxy.websocket,
            client_max_body_size: proxy.client_max_body_size,
            connect_timeout_secs: proxy.connect_timeout_seconds,
            read_timeout_secs: proxy.read_timeout_seconds,
            send_timeout_secs: proxy.send_timeout_seconds,
            request_headers: headers_from_pb(proxy.request_headers),
            response_headers: headers_from_pb(proxy.response_headers),
            upstream_host: proxy.upstream_host,
            no_buffering: proxy.no_buffering,
        },
        real_ip: match pb::SiteRealIp::try_from(p.real_ip) {
            Ok(pb::SiteRealIp::Cloudflare) => RealIp::Cloudflare,
            _ => RealIp::Off,
        },
        extra_server: p.extra_server,
        extra_location: p.extra_location,
        raw_config: p.raw_config.filter(|r| !r.trim().is_empty()),
    })
}

fn model_address_invalid(address: &str) -> bool {
    crate::daemon::webserver::model::validate_address(address).is_err()
}

/// Validation failures are the caller's fault.
fn web_status(err: anyhow::Error) -> Status {
    if err.downcast_ref::<users::UserError>().is_some() {
        return to_status(err);
    }
    let msg = format!("{err:#}");
    if msg.contains("nginx rejected")
        || msg.contains("not installed")
        || msg.contains("already installed")
    {
        Status::failed_precondition(msg)
    } else if msg.contains("not found") {
        Status::not_found(msg)
    } else if msg.contains("must")
        || msg.contains("invalid")
        || msg.contains("not a valid")
        || msg.contains("needs")
        || msg.contains("does not match")
    {
        Status::invalid_argument(msg)
    } else {
        Status::internal(msg)
    }
}

// ── gRPC ────────────────────────────────────────────────────────────────────

#[tonic::async_trait]
impl WebServerService for Grpc {
    type InstallWebServerStreamStream =
        Pin<Box<dyn Stream<Item = Result<pb::InstallWebServerEvent, Status>> + Send>>;

    async fn get_web_server(
        &self,
        request: Request<pb::GetWebServerRequest>,
    ) -> Result<GrpcResponse<pb::GetWebServerResponse>, Status> {
        let ctx = ctx_of(&request);
        let overview = self
            .0
            .web(&ctx, |w| Ok(w.overview()))
            .await
            .map_err(web_status)?;
        Ok(GrpcResponse::new(overview_to_pb(&overview)))
    }

    async fn install_web_server_stream(
        &self,
        request: Request<pb::InstallWebServerRequest>,
    ) -> Result<GrpcResponse<Self::InstallWebServerStreamStream>, Status> {
        let ctx = ctx_of(&request);
        let mode = mode_from_pb(request.into_inner().mode)?;
        let rx = self.0.web_install_stream(ctx, mode);
        let stream = futures_util::stream::unfold(rx, |mut rx| async move {
            let event = match rx.recv().await? {
                InstallEvent::Line(line) => pb::install_web_server_event::Event::Log(line),
                InstallEvent::Done(Ok(overview)) => {
                    pb::install_web_server_event::Event::Done(overview_to_pb(&overview))
                }
                InstallEvent::Done(Err(err)) => {
                    pb::install_web_server_event::Event::Error(format!("{err:#}"))
                }
            };
            Some((Ok(pb::InstallWebServerEvent { event: Some(event) }), rx))
        });
        Ok(GrpcResponse::new(Box::pin(stream)))
    }

    async fn uninstall_web_server(
        &self,
        request: Request<pb::UninstallWebServerRequest>,
    ) -> Result<GrpcResponse<pb::UninstallWebServerResponse>, Status> {
        let ctx = ctx_of(&request);
        let purge = request.into_inner().purge;
        self.0
            .web(&ctx, move |w| w.uninstall(purge, &mut |_| {}))
            .await
            .map_err(web_status)?;
        Ok(GrpcResponse::new(pb::UninstallWebServerResponse {}))
    }

    async fn update_web_server_settings(
        &self,
        request: Request<pb::UpdateWebServerSettingsRequest>,
    ) -> Result<GrpcResponse<pb::UpdateWebServerSettingsResponse>, Status> {
        let ctx = ctx_of(&request);
        let settings = request
            .into_inner()
            .settings
            .ok_or_else(|| Status::invalid_argument("settings are required"))?;
        let settings = settings_from_pb(settings);
        let overview = self
            .0
            .web(&ctx, move |w| w.update_settings(settings))
            .await
            .map_err(web_status)?;
        Ok(GrpcResponse::new(pb::UpdateWebServerSettingsResponse {
            web_server: Some(overview_to_pb(&overview)),
        }))
    }

    async fn test_web_server_config(
        &self,
        request: Request<pb::TestWebServerConfigRequest>,
    ) -> Result<GrpcResponse<pb::TestWebServerConfigResponse>, Status> {
        let ctx = ctx_of(&request);
        let (ok, output) = self.0.web(&ctx, |w| w.test()).await.map_err(web_status)?;
        Ok(GrpcResponse::new(pb::TestWebServerConfigResponse {
            ok,
            output,
        }))
    }

    async fn reload_web_server(
        &self,
        request: Request<pb::ReloadWebServerRequest>,
    ) -> Result<GrpcResponse<pb::ReloadWebServerResponse>, Status> {
        let ctx = ctx_of(&request);
        self.0.web(&ctx, |w| w.reload()).await.map_err(web_status)?;
        Ok(GrpcResponse::new(pb::ReloadWebServerResponse {}))
    }

    async fn get_web_server_files(
        &self,
        request: Request<pb::GetWebServerFilesRequest>,
    ) -> Result<GrpcResponse<pb::GetWebServerFilesResponse>, Status> {
        let ctx = ctx_of(&request);
        let files = self.0.web(&ctx, |w| w.files()).await.map_err(web_status)?;
        Ok(GrpcResponse::new(pb::GetWebServerFilesResponse {
            files: files
                .into_iter()
                .map(|(path, content)| pb::WebServerFile { path, content })
                .collect(),
        }))
    }

    async fn list_sites(
        &self,
        request: Request<pb::ListSitesRequest>,
    ) -> Result<GrpcResponse<pb::ListSitesResponse>, Status> {
        let ctx = ctx_of(&request);
        let managed_by = request.into_inner().managed_by.filter(|m| !m.is_empty());
        let sites = self
            .0
            .web(&ctx, move |w| w.sites(managed_by.as_deref()))
            .await
            .map_err(web_status)?;
        Ok(GrpcResponse::new(pb::ListSitesResponse {
            sites: sites.into_iter().map(view_to_pb).collect(),
        }))
    }

    async fn replace_sites(
        &self,
        request: Request<pb::ReplaceSitesRequest>,
    ) -> Result<GrpcResponse<pb::ReplaceSitesResponse>, Status> {
        let ctx = ctx_of(&request);
        let req = request.into_inner();
        let sites = req
            .sites
            .into_iter()
            .map(site_from_pb)
            .collect::<Result<Vec<_>, _>>()?;
        let managed_by = req.managed_by;
        let views = self
            .0
            .web(&ctx, move |w| w.replace_sites(&managed_by, sites))
            .await
            .map_err(web_status)?;
        Ok(GrpcResponse::new(pb::ReplaceSitesResponse {
            sites: views.into_iter().map(view_to_pb).collect(),
        }))
    }

    async fn upsert_site(
        &self,
        request: Request<pb::UpsertSiteRequest>,
    ) -> Result<GrpcResponse<pb::UpsertSiteResponse>, Status> {
        let ctx = ctx_of(&request);
        let site = request
            .into_inner()
            .site
            .ok_or_else(|| Status::invalid_argument("site is required"))?;
        let site = site_from_pb(site)?;
        let view = self
            .0
            .web(&ctx, move |w| w.upsert_site(site))
            .await
            .map_err(web_status)?;
        Ok(GrpcResponse::new(pb::UpsertSiteResponse {
            site: Some(view_to_pb(view)),
        }))
    }

    async fn remove_site(
        &self,
        request: Request<pb::RemoveSiteRequest>,
    ) -> Result<GrpcResponse<pb::RemoveSiteResponse>, Status> {
        let ctx = ctx_of(&request);
        let id = request.into_inner().id;
        let removed = self
            .0
            .web(&ctx, move |w| w.remove_site(&id))
            .await
            .map_err(web_status)?;
        Ok(GrpcResponse::new(pb::RemoveSiteResponse { removed }))
    }

    async fn render_site(
        &self,
        request: Request<pb::RenderSiteRequest>,
    ) -> Result<GrpcResponse<pb::RenderSiteResponse>, Status> {
        let ctx = ctx_of(&request);
        let site = request
            .into_inner()
            .site
            .ok_or_else(|| Status::invalid_argument("site is required"))?;
        let site = site_from_pb(site)?;
        let content = self
            .0
            .web(&ctx, move |w| w.render_site(site))
            .await
            .map_err(web_status)?;
        Ok(GrpcResponse::new(pb::RenderSiteResponse { content }))
    }

    async fn probe_tcp(
        &self,
        request: Request<pb::ProbeTcpRequest>,
    ) -> Result<GrpcResponse<pb::ProbeTcpResponse>, Status> {
        let ctx = ctx_of(&request);
        let req = request.into_inner();
        if model_address_invalid(&req.address) {
            return Err(Status::invalid_argument("address must be host:port"));
        }
        let (reachable, latency_ms, error) = self
            .0
            .web(&ctx, move |w| Ok(w.probe_tcp(&req.address, req.timeout_ms)))
            .await
            .map_err(web_status)?;
        Ok(GrpcResponse::new(pb::ProbeTcpResponse {
            reachable,
            latency_ms,
            error,
        }))
    }

    async fn renew_certificate(
        &self,
        request: Request<pb::RenewCertificateRequest>,
    ) -> Result<GrpcResponse<pb::RenewCertificateResponse>, Status> {
        let ctx = ctx_of(&request);
        let id = request.into_inner().id;
        let view = self
            .0
            .web(&ctx, move |w| w.renew(&id))
            .await
            .map_err(web_status)?;
        Ok(GrpcResponse::new(pb::RenewCertificateResponse {
            site: Some(view_to_pb(view)),
        }))
    }
}

// ── REST (CLI) ──────────────────────────────────────────────────────────────

pub fn routes() -> Router<Arc<ApiState>> {
    Router::new()
        .route("/v1/webserver", get(rest_get).delete(rest_uninstall))
        .route("/v1/webserver/install", post(rest_install))
        .route("/v1/webserver/settings", put(rest_settings))
        .route("/v1/webserver/test", post(rest_test))
        .route("/v1/webserver/reload", post(rest_reload))
        .route("/v1/webserver/files", get(rest_files))
        .route("/v1/webserver/sites", get(rest_sites))
        .route(
            "/v1/webserver/sites/{id}",
            get(rest_site)
                .put(rest_upsert_site)
                .delete(rest_remove_site),
        )
        .route("/v1/webserver/sites/{id}/renew", post(rest_renew))
}

async fn rest_get(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
) -> Result<Response, ApiError> {
    let overview = state.web(&ctx, |w| Ok(w.overview())).await?;
    Ok(Json(overview).into_response())
}

#[derive(Deserialize)]
struct InstallBody {
    mode: String,
}

async fn rest_install(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Json(body): Json<InstallBody>,
) -> Result<Response, ApiError> {
    let mode = Mode::parse(&body.mode)?;
    let mut rx = state.web_install_stream(ctx, mode);
    let mut log = Vec::new();
    while let Some(event) = rx.recv().await {
        match event {
            InstallEvent::Line(line) => log.push(line),
            InstallEvent::Done(result) => {
                let overview = result?;
                return Ok(
                    Json(serde_json::json!({ "log": log, "webserver": overview })).into_response(),
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
        .web(&ctx, move |w| {
            let mut log = Vec::new();
            w.uninstall(query.purge, &mut |l| log.push(l.to_string()))?;
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
        .web(&ctx, move |w| w.update_settings(settings))
        .await?;
    Ok(Json(overview).into_response())
}

async fn rest_test(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
) -> Result<Response, ApiError> {
    let (ok, output) = state.web(&ctx, |w| w.test()).await?;
    Ok(Json(serde_json::json!({ "ok": ok, "output": output })).into_response())
}

async fn rest_reload(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
) -> Result<Response, ApiError> {
    state.web(&ctx, |w| w.reload()).await?;
    Ok(Json(serde_json::json!({})).into_response())
}

async fn rest_files(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
) -> Result<Response, ApiError> {
    let files = state.web(&ctx, |w| w.files()).await?;
    let files: Vec<_> = files
        .into_iter()
        .map(|(path, content)| serde_json::json!({ "path": path, "content": content }))
        .collect();
    Ok(Json(serde_json::json!({ "files": files })).into_response())
}

async fn rest_sites(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
) -> Result<Response, ApiError> {
    let sites = state.web(&ctx, |w| w.sites(None)).await?;
    Ok(Json(serde_json::json!({ "sites": sites })).into_response())
}

async fn rest_site(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let site = state.web(&ctx, move |w| w.site(&id)).await?;
    Ok(Json(site).into_response())
}

async fn rest_upsert_site(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Path(id): Path<String>,
    Json(mut site): Json<Site>,
) -> Result<Response, ApiError> {
    site.id = id;
    let view = state.web(&ctx, move |w| w.upsert_site(site)).await?;
    Ok(Json(view).into_response())
}

async fn rest_remove_site(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let removed = state.web(&ctx, move |w| w.remove_site(&id)).await?;
    Ok(Json(serde_json::json!({ "removed": removed })).into_response())
}

async fn rest_renew(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let view = state.web(&ctx, move |w| w.renew(&id)).await?;
    Ok(Json(view).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_round_trips_through_proto_without_keys() {
        let site = Site {
            id: "s".into(),
            server_names: vec!["a.example.com".into()],
            managed_by: Some("platform".into()),
            disabled: false,
            upstream: Upstream {
                servers: vec![UpstreamServer {
                    target: Target::App {
                        app: "grafana".into(),
                        port: 3000,
                    },
                    weight: 2,
                    backup: false,
                    max_fails: 1,
                    fail_timeout_secs: 5,
                    down: false,
                }],
                balance: Balance::LeastConn,
                ..Default::default()
            },
            tls: Tls {
                mode: TlsMode::Provided,
                certificate_pem: "C".into(),
                private_key_pem: "K".into(),
                redirect_http: true,
                hsts: true,
                http2: true,
            },
            proxy: Proxy {
                websocket: true,
                ..Default::default()
            },
            real_ip: RealIp::Cloudflare,
            extra_server: "x".into(),
            extra_location: String::new(),
            raw_config: None,
        };
        let back = site_from_pb(site_to_pb(site.clone())).unwrap();
        assert_eq!(
            back.tls.certificate_pem, "",
            "keys never leave through the API"
        );
        let mut expected = site;
        expected.tls.certificate_pem.clear();
        expected.tls.private_key_pem.clear();
        assert_eq!(back, expected);
    }
}
