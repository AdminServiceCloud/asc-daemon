//! WireGuard API: the service layer over
//! [`crate::daemon::wireguard::Wireguard`] shared by both transports, the
//! gRPC `WireguardService` and the REST routes the CLI uses. Every call is
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
use crate::daemon::wireguard::{
    ClientInput, ImportInput, InterfaceInput, Overview, PeerAdd, PeerUpdate, Wireguard,
};

use pb::wireguard_service_server::WireguardService;

// ── Service layer ───────────────────────────────────────────────────────────

impl ApiState {
    async fn wgd<T, F>(self: &Arc<Self>, ctx: &UserContext, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Wireguard) -> Result<T> + Send + 'static,
    {
        users::require_root(ctx)?;
        let state = Arc::clone(self);
        tokio::task::spawn_blocking(move || f(&state.wireguard))
            .await
            .context("wireguard worker panicked")?
    }

    /// Installs WireGuard with progress lines on the channel; the last
    /// message is the result.
    fn wgd_install_stream(
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
            let result = state.wireguard.install(&mut progress).map(Box::new);
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

fn state_to_pb(o: &Overview) -> pb::WireguardState {
    pb::WireguardState {
        installed: o.installed,
        version: o.version.clone(),
        interfaces: o
            .interfaces
            .iter()
            .map(|i| pb::WireguardInterface {
                name: i.name.clone(),
                listen_port: u32::from(i.listen_port.unwrap_or(0)),
                addresses: i.addresses.clone(),
                dns: i.dns.clone(),
                client_dns: i.client_dns.clone(),
                mtu: u32::from(i.mtu.unwrap_or(0)),
                public_key: i.public_key.clone(),
                endpoint: i.endpoint.clone(),
                masquerade: i.masquerade,
                running: i.running,
                enabled: i.enabled,
                hooks: i.hooks.clone(),
                table: i.table.clone(),
                fwmark: i.fwmark.clone(),
                peers: i
                    .peers
                    .iter()
                    .map(|p| pb::WireguardPeer {
                        public_key: p.peer.public_key.clone(),
                        name: p.peer.name.clone(),
                        enabled: p.peer.enabled,
                        allowed_ips: p.peer.allowed_ips.clone(),
                        endpoint: p.peer.endpoint.clone(),
                        persistent_keepalive: u32::from(p.peer.persistent_keepalive),
                        has_preshared_key: p.has_preshared_key,
                        live: p.live.as_ref().map(|l| pb::WireguardLive {
                            endpoint: l.endpoint.clone(),
                            latest_handshake_unix: l.latest_handshake_unix,
                            rx_bytes: l.rx_bytes,
                            tx_bytes: l.tx_bytes,
                        }),
                    })
                    .collect(),
            })
            .collect(),
        unmanaged: o
            .unmanaged
            .iter()
            .map(|u| pb::WireguardUnmanaged {
                name: u.name.clone(),
                running: u.running,
                error: u.error.clone(),
            })
            .collect(),
        primary_address: o.primary_address.clone(),
    }
}

/// 0 means "not set"; anything above `u16::MAX` is a caller's mistake.
fn small(value: u32, what: &str) -> Result<Option<u16>, Status> {
    match value {
        0 => Ok(None),
        v => u16::try_from(v)
            .map(Some)
            .map_err(|_| Status::invalid_argument(format!("{what} is out of range"))),
    }
}

fn client_from_pb(client: Option<pb::WireguardClientOptions>) -> ClientInput {
    let client = client.unwrap_or_default();
    ClientInput {
        routes: client.routes,
        dns: client.dns,
        endpoint: client.endpoint,
    }
}

fn interface_from_pb(req: pb::UpsertWireguardInterfaceRequest) -> Result<InterfaceInput, Status> {
    Ok(InterfaceInput {
        name: req.name,
        listen_port: small(req.listen_port, "the listen port")?,
        addresses: req.addresses,
        dns: req.dns,
        client_dns: req.client_dns,
        mtu: small(req.mtu, "the MTU")?,
        endpoint: req.endpoint,
        masquerade: req.masquerade,
        private_key: req.private_key,
    })
}

// ── gRPC ────────────────────────────────────────────────────────────────────

#[tonic::async_trait]
impl WireguardService for Grpc {
    type InstallWireguardStreamStream =
        Pin<Box<dyn Stream<Item = Result<pb::WireguardInstallEvent, Status>> + Send>>;

    async fn get_wireguard(
        &self,
        request: Request<pb::GetWireguardRequest>,
    ) -> Result<GrpcResponse<pb::GetWireguardResponse>, Status> {
        let ctx = ctx_of(&request);
        let overview = self
            .0
            .wgd(&ctx, |w| w.overview())
            .await
            .map_err(to_status)?;
        Ok(GrpcResponse::new(pb::GetWireguardResponse {
            wireguard: Some(state_to_pb(&overview)),
        }))
    }

    async fn install_wireguard_stream(
        &self,
        request: Request<pb::InstallWireguardRequest>,
    ) -> Result<GrpcResponse<Self::InstallWireguardStreamStream>, Status> {
        let ctx = ctx_of(&request);
        let rx = self.0.wgd_install_stream(ctx);
        let stream = futures_util::stream::unfold(rx, |mut rx| async move {
            let event = match rx.recv().await? {
                InstallEvent::Line(line) => pb::wireguard_install_event::Event::Log(line),
                InstallEvent::Done(Ok(overview)) => {
                    pb::wireguard_install_event::Event::Done(state_to_pb(&overview))
                }
                InstallEvent::Done(Err(err)) => {
                    pb::wireguard_install_event::Event::Error(format!("{err:#}"))
                }
            };
            Some((Ok(pb::WireguardInstallEvent { event: Some(event) }), rx))
        });
        Ok(GrpcResponse::new(Box::pin(stream)))
    }

    async fn uninstall_wireguard(
        &self,
        request: Request<pb::UninstallWireguardRequest>,
    ) -> Result<GrpcResponse<pb::UninstallWireguardResponse>, Status> {
        let ctx = ctx_of(&request);
        let purge = request.into_inner().purge;
        self.0
            .wgd(&ctx, move |w| w.uninstall(purge, &mut |_| {}))
            .await
            .map_err(to_status)?;
        Ok(GrpcResponse::new(pb::UninstallWireguardResponse {}))
    }

    async fn upsert_wireguard_interface(
        &self,
        request: Request<pb::UpsertWireguardInterfaceRequest>,
    ) -> Result<GrpcResponse<pb::UpsertWireguardInterfaceResponse>, Status> {
        let ctx = ctx_of(&request);
        let input = interface_from_pb(request.into_inner())?;
        let overview = self
            .0
            .wgd(&ctx, move |w| w.upsert_interface(input))
            .await
            .map_err(to_status)?;
        Ok(GrpcResponse::new(pb::UpsertWireguardInterfaceResponse {
            wireguard: Some(state_to_pb(&overview)),
        }))
    }

    async fn remove_wireguard_interface(
        &self,
        request: Request<pb::RemoveWireguardInterfaceRequest>,
    ) -> Result<GrpcResponse<pb::RemoveWireguardInterfaceResponse>, Status> {
        let ctx = ctx_of(&request);
        let name = request.into_inner().name;
        let overview = self
            .0
            .wgd(&ctx, move |w| w.remove_interface(&name))
            .await
            .map_err(to_status)?;
        Ok(GrpcResponse::new(pb::RemoveWireguardInterfaceResponse {
            wireguard: Some(state_to_pb(&overview)),
        }))
    }

    async fn set_wireguard_interface_state(
        &self,
        request: Request<pb::SetWireguardInterfaceStateRequest>,
    ) -> Result<GrpcResponse<pb::SetWireguardInterfaceStateResponse>, Status> {
        let ctx = ctx_of(&request);
        let req = request.into_inner();
        let overview = self
            .0
            .wgd(&ctx, move |w| w.set_state(&req.name, req.up))
            .await
            .map_err(to_status)?;
        Ok(GrpcResponse::new(pb::SetWireguardInterfaceStateResponse {
            wireguard: Some(state_to_pb(&overview)),
        }))
    }

    async fn get_wireguard_interface_config(
        &self,
        request: Request<pb::GetWireguardInterfaceConfigRequest>,
    ) -> Result<GrpcResponse<pb::GetWireguardInterfaceConfigResponse>, Status> {
        let ctx = ctx_of(&request);
        let name = request.into_inner().name;
        let config = self
            .0
            .wgd(&ctx, move |w| w.interface_config(&name))
            .await
            .map_err(to_status)?;
        Ok(GrpcResponse::new(pb::GetWireguardInterfaceConfigResponse {
            config,
        }))
    }

    async fn import_wireguard_config(
        &self,
        request: Request<pb::ImportWireguardConfigRequest>,
    ) -> Result<GrpcResponse<pb::ImportWireguardConfigResponse>, Status> {
        let ctx = ctx_of(&request);
        let req = request.into_inner();
        let input = ImportInput {
            name: req.name,
            text: req.text,
            overwrite: req.overwrite,
            accept_hooks: req.accept_hooks,
            start: req.start,
        };
        let done = self
            .0
            .wgd(&ctx, move |w| w.import(input))
            .await
            .map_err(to_status)?;
        Ok(GrpcResponse::new(pb::ImportWireguardConfigResponse {
            wireguard: Some(state_to_pb(&done.overview)),
            name: done.name,
            warnings: done.warnings,
        }))
    }

    async fn add_wireguard_peer(
        &self,
        request: Request<pb::AddWireguardPeerRequest>,
    ) -> Result<GrpcResponse<pb::AddWireguardPeerResponse>, Status> {
        let ctx = ctx_of(&request);
        let req = request.into_inner();
        let add = PeerAdd {
            interface: req.interface,
            name: req.name,
            public_key: req.public_key,
            allowed_ips: req.allowed_ips,
            preshared: req.preshared,
            persistent_keepalive: small(req.persistent_keepalive, "the keepalive")?.unwrap_or(0),
            endpoint: req.endpoint,
            client: client_from_pb(req.client),
        };
        let added = self
            .0
            .wgd(&ctx, move |w| w.add_peer(add))
            .await
            .map_err(to_status)?;
        Ok(GrpcResponse::new(pb::AddWireguardPeerResponse {
            wireguard: Some(state_to_pb(&added.overview)),
            public_key: added.public_key,
            client_config: added.client_config,
        }))
    }

    async fn update_wireguard_peer(
        &self,
        request: Request<pb::UpdateWireguardPeerRequest>,
    ) -> Result<GrpcResponse<pb::UpdateWireguardPeerResponse>, Status> {
        let ctx = ctx_of(&request);
        let req = request.into_inner();
        let update = PeerUpdate {
            interface: req.interface,
            peer: req.peer,
            name: req.name,
            allowed_ips: req.allowed_ips,
            endpoint: req.endpoint,
            persistent_keepalive: small(req.persistent_keepalive, "the keepalive")?.unwrap_or(0),
            enabled: req.enabled,
        };
        let overview = self
            .0
            .wgd(&ctx, move |w| w.update_peer(update))
            .await
            .map_err(to_status)?;
        Ok(GrpcResponse::new(pb::UpdateWireguardPeerResponse {
            wireguard: Some(state_to_pb(&overview)),
        }))
    }

    async fn remove_wireguard_peer(
        &self,
        request: Request<pb::RemoveWireguardPeerRequest>,
    ) -> Result<GrpcResponse<pb::RemoveWireguardPeerResponse>, Status> {
        let ctx = ctx_of(&request);
        let req = request.into_inner();
        let overview = self
            .0
            .wgd(&ctx, move |w| w.remove_peer(&req.interface, &req.peer))
            .await
            .map_err(to_status)?;
        Ok(GrpcResponse::new(pb::RemoveWireguardPeerResponse {
            wireguard: Some(state_to_pb(&overview)),
        }))
    }

    async fn get_wireguard_peer_config(
        &self,
        request: Request<pb::GetWireguardPeerConfigRequest>,
    ) -> Result<GrpcResponse<pb::GetWireguardPeerConfigResponse>, Status> {
        let ctx = ctx_of(&request);
        let req = request.into_inner();
        let client = client_from_pb(req.client);
        let config = self
            .0
            .wgd(&ctx, move |w| {
                w.peer_config(&req.interface, &req.peer, &client)
            })
            .await
            .map_err(to_status)?;
        Ok(GrpcResponse::new(pb::GetWireguardPeerConfigResponse {
            config,
        }))
    }
}

// ── REST (CLI) ──────────────────────────────────────────────────────────────

pub fn routes() -> Router<Arc<ApiState>> {
    Router::new()
        .route("/v1/wireguard", get(rest_get).delete(rest_uninstall))
        .route("/v1/wireguard/install", post(rest_install))
        .route("/v1/wireguard/import", post(rest_import))
        .route(
            "/v1/wireguard/interfaces/{name}",
            put(rest_interface).delete(rest_interface_remove),
        )
        .route("/v1/wireguard/interfaces/{name}/state", post(rest_state))
        .route(
            "/v1/wireguard/interfaces/{name}/config",
            get(rest_interface_config),
        )
        // A public key is base64 and may hold `/`, so peers travel in the body.
        .route(
            "/v1/wireguard/peers",
            post(rest_peer_add).put(rest_peer_update),
        )
        .route("/v1/wireguard/peers/remove", post(rest_peer_remove))
        .route("/v1/wireguard/peers/config", post(rest_peer_config))
}

async fn rest_get(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
) -> Result<Response, ApiError> {
    let overview = state.wgd(&ctx, |w| w.overview()).await?;
    Ok(Json(overview).into_response())
}

async fn rest_install(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
) -> Result<Response, ApiError> {
    let mut rx = state.wgd_install_stream(ctx);
    let mut log = Vec::new();
    while let Some(event) = rx.recv().await {
        match event {
            InstallEvent::Line(line) => log.push(line),
            InstallEvent::Done(result) => {
                let overview = result?;
                return Ok(
                    Json(serde_json::json!({ "log": log, "wireguard": overview })).into_response(),
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
        .wgd(&ctx, move |w| {
            let mut log = Vec::new();
            w.uninstall(query.purge, &mut |l| log.push(l.to_string()))?;
            Ok(log)
        })
        .await?;
    Ok(Json(serde_json::json!({ "log": log })).into_response())
}

async fn rest_import(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Json(input): Json<ImportInput>,
) -> Result<Response, ApiError> {
    let done = state.wgd(&ctx, move |w| w.import(input)).await?;
    Ok(Json(done).into_response())
}

async fn rest_interface(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Path(name): Path<String>,
    Json(mut input): Json<InterfaceInput>,
) -> Result<Response, ApiError> {
    input.name = name;
    let overview = state.wgd(&ctx, move |w| w.upsert_interface(input)).await?;
    Ok(Json(overview).into_response())
}

async fn rest_interface_remove(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    let overview = state.wgd(&ctx, move |w| w.remove_interface(&name)).await?;
    Ok(Json(overview).into_response())
}

#[derive(Deserialize)]
struct StateBody {
    up: bool,
}

async fn rest_state(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Path(name): Path<String>,
    Json(body): Json<StateBody>,
) -> Result<Response, ApiError> {
    let overview = state
        .wgd(&ctx, move |w| w.set_state(&name, body.up))
        .await?;
    Ok(Json(overview).into_response())
}

async fn rest_interface_config(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    let config = state.wgd(&ctx, move |w| w.interface_config(&name)).await?;
    Ok(Json(serde_json::json!({ "config": config })).into_response())
}

async fn rest_peer_add(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Json(add): Json<PeerAdd>,
) -> Result<Response, ApiError> {
    let added = state.wgd(&ctx, move |w| w.add_peer(add)).await?;
    Ok(Json(added).into_response())
}

async fn rest_peer_update(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Json(update): Json<PeerUpdate>,
) -> Result<Response, ApiError> {
    let overview = state.wgd(&ctx, move |w| w.update_peer(update)).await?;
    Ok(Json(overview).into_response())
}

#[derive(Deserialize)]
struct PeerRef {
    interface: String,
    peer: String,
    #[serde(default)]
    client: ClientInput,
}

async fn rest_peer_remove(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Json(body): Json<PeerRef>,
) -> Result<Response, ApiError> {
    let overview = state
        .wgd(&ctx, move |w| w.remove_peer(&body.interface, &body.peer))
        .await?;
    Ok(Json(overview).into_response())
}

async fn rest_peer_config(
    State(state): State<Arc<ApiState>>,
    Extension(ctx): Extension<UserContext>,
    Json(body): Json<PeerRef>,
) -> Result<Response, ApiError> {
    let config = state
        .wgd(&ctx, move |w| {
            w.peer_config(&body.interface, &body.peer, &body.client)
        })
        .await?;
    Ok(Json(serde_json::json!({ "config": config })).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_means_unset_and_oversized_numbers_are_refused() {
        assert_eq!(small(0, "x").unwrap(), None);
        assert_eq!(small(51820, "x").unwrap(), Some(51820));
        assert_eq!(small(65535, "x").unwrap(), Some(65535));
        assert_eq!(
            small(70000, "the port").unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
    }

    #[test]
    fn an_interface_request_maps_zero_port_to_a_client_style_tunnel() {
        let input = interface_from_pb(pb::UpsertWireguardInterfaceRequest {
            name: "wg0".into(),
            addresses: vec!["10.8.0.1/24".into()],
            ..Default::default()
        })
        .unwrap();
        assert_eq!(input.listen_port, None);
        assert_eq!(input.mtu, None);
    }

    #[test]
    fn client_options_survive_the_proto() {
        let client = client_from_pb(Some(pb::WireguardClientOptions {
            routes: "full".into(),
            dns: vec!["9.9.9.9".into()],
            endpoint: "vpn.example.com".into(),
        }));
        assert_eq!(client.routes, "full");
        assert_eq!(client.dns, ["9.9.9.9"]);
        assert_eq!(client_from_pb(None).routes, "");
    }
}
