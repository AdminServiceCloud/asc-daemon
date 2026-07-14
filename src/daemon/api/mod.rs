//! Daemon API (DMN-005): gRPC (tonic; wire-compatible with the platform's
//! ConnectRPC clients) and REST (JSON) served **together on one listener**,
//! both calling the same service layer and sharing bearer-token auth.
//!
//! Remote access goes through the platform tunnel; locally the API listens
//! on localhost only (config `[api] listen`).

pub mod console;
mod grpc;
pub mod proto;
mod rest;
mod ws;

use std::future::Future;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use tracing::info;

use crate::daemon::apps::{AppManager, AppStatus, Outcome, UserContext};
use crate::daemon::config::Config;
use crate::daemon::monitor::Monitor;
use crate::daemon::pkg;

use console::ConsoleTokens;

/// Shared state behind both transports.
pub struct ApiState {
    pub config: Config,
    pub manager: AppManager,
    pub console_tokens: ConsoleTokens,
    /// Shared attach sessions: one source per app, many console clients.
    pub attach_hub: crate::daemon::console::hub::AttachHub,
    /// System metrics ring buffer, filled by the daemon's sampler task.
    pub monitor: Arc<Monitor>,
    /// Bearer token required on every request.
    token: String,
}

/// API calls act with full visibility: the platform performs its own
/// per-user permission checks before reaching the daemon. Per-user API
/// tokens are a follow-up (see docs/api.md).
fn api_context() -> UserContext {
    UserContext {
        uid: 0,
        name: "api".into(),
        is_root: true,
    }
}

impl ApiState {
    pub fn new(config: Config, token: String) -> Arc<Self> {
        let monitor = Monitor::new(&config.monitor);
        Arc::new(Self {
            manager: AppManager::new(&config),
            config,
            console_tokens: ConsoleTokens::default(),
            attach_hub: Default::default(),
            monitor,
            token,
        })
    }

    // ── Service layer: blocking app operations moved off the async runtime ──

    async fn blocking<T, F>(self: &Arc<Self>, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&ApiState) -> Result<T> + Send + 'static,
    {
        let state = Arc::clone(self);
        tokio::task::spawn_blocking(move || f(&state))
            .await
            .context("api worker task panicked")?
    }

    pub async fn status(self: &Arc<Self>) -> Result<(usize, usize)> {
        self.blocking(|s| {
            let apps = s.manager.list(&api_context())?;
            let running = apps
                .iter()
                .filter(|a| a.state == crate::daemon::apps::RuntimeState::Running)
                .count();
            Ok((running, apps.len()))
        })
        .await
    }

    pub async fn list_apps(self: &Arc<Self>) -> Result<Vec<AppStatus>> {
        self.blocking(|s| s.manager.list(&api_context())).await
    }

    pub async fn get_app(self: &Arc<Self>, id: String) -> Result<AppStatus> {
        self.blocking(move |s| s.manager.status(&api_context(), &id))
            .await
    }

    pub async fn install(
        self: &Arc<Self>,
        spec: String,
        source: Option<String>,
    ) -> Result<pkg::InstallOutcome> {
        self.blocking(move |s| {
            // No license consent over the API yet: a repository shipping a
            // LICENSE returns the typed error, and the platform UI will
            // render its own consent dialog from it (DMN-028 follow-up).
            pkg::install(
                &s.config,
                &api_context(),
                &spec,
                source.as_deref(),
                None,
                false,
            )
        })
        .await
    }

    pub async fn start(self: &Arc<Self>, id: String) -> Result<Outcome> {
        self.blocking(move |s| s.manager.start(&api_context(), &id))
            .await
    }

    pub async fn stop(self: &Arc<Self>, id: String) -> Result<Outcome> {
        self.blocking(move |s| s.manager.stop(&api_context(), &id))
            .await
    }

    pub async fn restart(self: &Arc<Self>, id: String) -> Result<()> {
        self.blocking(move |s| s.manager.restart(&api_context(), &id))
            .await
    }

    pub async fn logs(self: &Arc<Self>, id: String, tail: usize) -> Result<String> {
        self.blocking(move |s| s.manager.logs(&api_context(), &id, tail))
            .await
    }

    pub async fn remove(self: &Arc<Self>, id: String) -> Result<()> {
        self.blocking(move |s| s.manager.remove(&api_context(), &id))
            .await
    }

    /// Issue a one-time console token after verifying the app exists.
    pub async fn issue_console_token(
        self: &Arc<Self>,
        app_id: String,
        session: console::SessionType,
    ) -> Result<(String, i64)> {
        let id = app_id.clone();
        // Existence + authorization check first: no tokens for unknown apps.
        self.blocking(move |s| s.manager.get_authorized(&api_context(), &id))
            .await?;
        Ok(self.console_tokens.issue(&app_id, session))
    }
}

/// The API bearer token file, next to config.toml (`/etc/asc/api.token`).
///
/// Kept out of config.toml on purpose: the config is world-readable (users
/// need the language and [policy] settings), the token is root-only (0600).
pub fn api_token_path() -> std::path::PathBuf {
    Config::path().with_file_name("api.token")
}

/// Ensure the API token exists, generating and persisting one on first run.
/// A legacy token found inside config.toml (pre-split installs) is migrated
/// into the token file and removed from the config.
pub fn ensure_api_token(config: &mut Config) -> Result<String> {
    let path = api_token_path();
    if let Some(token) = config.api.token.take() {
        write_token(&path, &token)?;
        config
            .save()
            .context("cannot rewrite config.toml after token migration")?;
        info!("migrated API token from config.toml to api.token");
        return Ok(token);
    }
    match std::fs::read_to_string(&path) {
        Ok(raw) => Ok(raw.trim().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let token = console::random_hex(32);
            write_token(&path, &token)?;
            info!(file = %path.display(), "generated API token");
            Ok(token)
        }
        Err(e) => Err(e).with_context(|| format!("cannot read token file {}", path.display())),
    }
}

/// Write the token file with root-only permissions.
fn write_token(path: &std::path::Path, token: &str) -> Result<()> {
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("cannot create directory {}", dir.display()))?;
    }
    std::fs::write(path, token)
        .with_context(|| format!("cannot write token file {}", path.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("cannot set permissions on {}", path.display()))?;
    }
    Ok(())
}

/// The full API router: REST + gRPC behind one auth middleware, plus the
/// WebSocket console, which sits outside bearer auth on purpose — browsers
/// cannot set headers on WS handshakes, so it is guarded by one-time
/// console tokens instead (issued via `IssueConsoleToken`).
pub fn router(state: Arc<ApiState>) -> Router {
    let grpc = grpc::routes(Arc::clone(&state));
    let auth_state = Arc::clone(&state);
    rest::router(Arc::clone(&state))
        .merge(grpc)
        .layer(middleware::from_fn(move |req, next| {
            let state = Arc::clone(&auth_state);
            auth(state, req, next)
        }))
        .merge(ws::router(state))
}

/// Serve the API until `shutdown` resolves.
pub async fn serve(
    state: Arc<ApiState>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let listen = state.config.api.listen.clone();
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("cannot bind API listener on {listen}"))?;
    info!(addr = %listen, "API listening (gRPC + REST)");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await
        .context("API server failed")
}

/// Bearer-token check for both transports. gRPC callers get a proper
/// `grpc-status: UNAUTHENTICATED` trailer-only response, REST callers 401.
async fn auth(state: Arc<ApiState>, req: Request<Body>, next: Next) -> Response {
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if presented.is_some_and(|t| console::constant_time_eq(t, &state.token)) {
        return next.run(req).await;
    }
    if is_grpc(req.headers()) {
        // 16 = UNAUTHENTICATED
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/grpc")
            .header("grpc-status", "16")
            .header("grpc-message", "invalid or missing API token")
            .body(Body::empty())
            .expect("static response")
    } else {
        Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header("content-type", "application/json")
            .body(Body::from(r#"{"error":"invalid or missing API token"}"#))
            .expect("static response")
    }
}

fn is_grpc(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/grpc"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generation, reuse and legacy migration of the API token. One test —
    /// it owns the `ASC_CONFIG` env var (parallel tests must not race it).
    #[test]
    fn api_token_lifecycle() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        unsafe { std::env::set_var("ASC_CONFIG", &config_path) };

        // First start: token generated, file is root-only.
        let mut config = Config::default();
        let token = ensure_api_token(&mut config).unwrap();
        assert_eq!(token.len(), 64);
        let token_path = api_token_path();
        assert_eq!(token_path, dir.path().join("api.token"));
        let mode = std::fs::metadata(&token_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        // Second start: the same token is reused.
        let mut config = Config::default();
        assert_eq!(ensure_api_token(&mut config).unwrap(), token);

        // Legacy config with an embedded token: migrated out on start.
        std::fs::remove_file(&token_path).unwrap();
        let mut config = Config::default();
        config.api.token = Some("legacy-token".into());
        assert_eq!(ensure_api_token(&mut config).unwrap(), "legacy-token");
        assert_eq!(
            std::fs::read_to_string(&token_path).unwrap().trim(),
            "legacy-token"
        );
        // The rewritten config no longer contains the token and is 0644.
        let raw = std::fs::read_to_string(&config_path).unwrap();
        assert!(!raw.contains("legacy-token"));
        let mode = std::fs::metadata(&config_path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o644);

        unsafe { std::env::remove_var("ASC_CONFIG") };
    }
}
