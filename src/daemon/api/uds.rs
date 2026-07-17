//! Local unix-socket API listener (DMN-042): the same REST + gRPC router as
//! the TCP listener, but authenticated by **SO_PEERCRED** instead of the
//! bearer token — the kernel reports the connecting process's uid, and the
//! daemon builds the per-user [`UserContext`] from it on its own side.
//! Nothing inside the request can escalate: a regular user sees and manages
//! only their own apps, root (and `sudo asc`) everyone's.
//!
//! The socket itself is world-connectable (0666): reaching it grants
//! nothing — authorization is the peer uid, enforced per request. This is
//! what lets `asc install`/`asc ls` work without the `docker` group or sudo.

use std::future::Future;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::{self, ConnectInfo};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::serve::IncomingStream;
use tokio::net::UnixListener;
use tracing::info;

use crate::daemon::apps::UserContext;

use super::{ApiState, rest, ws};

/// The client's sudo attribution hint (see [`UserContext::from_peer`]):
/// honored only when the peer itself is root.
pub const SUDO_UID_HEADER: &str = "x-asc-sudo-uid";
pub const SUDO_USER_HEADER: &str = "x-asc-sudo-user";

/// Peer credentials captured at accept time, before hyper takes the stream.
#[derive(Clone, Debug)]
struct PeerCred {
    /// `None` when the kernel query failed — such requests are rejected.
    uid: Option<u32>,
}

impl connect_info::Connected<IncomingStream<'_, UnixListener>> for PeerCred {
    fn connect_info(stream: IncomingStream<'_, UnixListener>) -> Self {
        Self {
            uid: stream.io().peer_cred().ok().map(|cred| cred.uid()),
        }
    }
}

/// Build the request's [`UserContext`] from the socket peer credentials and
/// stamp it into the extensions — the same slot the TCP bearer middleware
/// fills with the full-visibility context.
async fn peer_auth(mut req: Request<Body>, next: Next) -> Response {
    let uid = req
        .extensions()
        .get::<ConnectInfo<PeerCred>>()
        .and_then(|info| info.uid);
    let Some(uid) = uid else {
        return Response::builder()
            .status(StatusCode::FORBIDDEN)
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"error":"cannot read peer credentials on the unix socket"}"#,
            ))
            .expect("static response");
    };
    let (sudo_uid, sudo_user) = sudo_hint(req.headers());
    let ctx = UserContext::from_peer(uid, sudo_uid, sudo_user.as_deref());
    req.extensions_mut().insert(ctx);
    next.run(req).await
}

/// The `X-Asc-Sudo-Uid` / `X-Asc-Sudo-User` attribution hint, if presented.
fn sudo_hint(headers: &HeaderMap) -> (Option<u32>, Option<String>) {
    let uid = headers
        .get(SUDO_UID_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    let user = headers
        .get(SUDO_USER_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    (uid, user)
}

/// REST + gRPC + WebSocket console behind peer-credential auth.
fn router(state: Arc<ApiState>) -> Router {
    rest::router(Arc::clone(&state))
        .merge(super::grpc::routes(Arc::clone(&state)))
        .layer(middleware::from_fn(peer_auth))
        .merge(ws::router(state))
}

/// Serve the API on the configured unix socket until `shutdown` resolves.
/// A stale socket file (unclean daemon exit) is replaced on startup.
pub async fn serve(
    state: Arc<ApiState>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let path = state.config.api.socket.clone();
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("cannot create socket directory {}", dir.display()))?;
    }
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e)
                .with_context(|| format!("cannot remove stale socket {}", path.display()));
        }
    }
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("cannot bind unix socket {}", path.display()))?;
    // World-connectable on purpose: the peer uid is the authorization.
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))
            .with_context(|| format!("cannot set permissions on {}", path.display()))?;
    }
    info!(path = %path.display(), "local API listening (unix socket, peer-cred auth)");
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<PeerCred>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
    .context("unix-socket API server failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sudo_hint_parses_headers() {
        let mut headers = HeaderMap::new();
        assert_eq!(sudo_hint(&headers), (None, None));
        headers.insert(SUDO_UID_HEADER, "1000".parse().unwrap());
        headers.insert(SUDO_USER_HEADER, "alice".parse().unwrap());
        assert_eq!(sudo_hint(&headers), (Some(1000), Some("alice".into())));
        headers.insert(SUDO_UID_HEADER, "not-a-uid".parse().unwrap());
        assert_eq!(sudo_hint(&headers).0, None);
    }
}
