//! API integration tests: REST and gRPC against an in-process server,
//! bearer-token auth on both transports, console-token issuing.

use std::sync::Arc;

use asc_daemon::daemon::api::proto::v1 as pb;
use asc_daemon::daemon::api::{self, ApiState};
use asc_daemon::daemon::apps::AppStore;
use asc_daemon::daemon::apps::meta::{AppMeta, DesiredState, Owner, Runtime};
use asc_daemon::daemon::config::Config;

const TOKEN: &str = "test-token-1234";

fn test_state() -> (Arc<ApiState>, tempfile::TempDir) {
    let ws = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.daemon.data_dir = ws.path().join("data");
    config.daemon.apps_dir = ws.path().join("apps");
    (ApiState::new(config, TOKEN.into()), ws)
}

fn install_fake_app(state: &ApiState, id: &str) {
    AppStore::new(state.config.daemon.apps_dir.clone())
        .save(&AppMeta {
            id: id.into(),
            name: id.into(),
            owner: Owner {
                uid: 0,
                name: "root".into(),
            },
            version: Some("v1.0.0".into()),
            source: Some("test:local".into()),
            desired_state: DesiredState::Stopped,
            runtime: Runtime::Process {
                command: "true".into(),
                args: vec![],
            },
        })
        .unwrap();
}

/// A fake metrics sample pushed straight into the ring buffer, standing in
/// for the daemon's background sampler.
fn fake_metrics(timestamp: i64) -> asc_daemon::daemon::monitor::SystemMetrics {
    use asc_daemon::daemon::monitor::system::*;
    SystemMetrics {
        timestamp,
        cpu: CpuMetrics {
            usage_percent: Some(12.5),
            cores: 4,
            load1: 0.5,
            load5: 0.4,
            load15: 0.3,
        },
        memory: MemoryMetrics {
            total: 8 * 1024 * 1024 * 1024,
            used: 2 * 1024 * 1024 * 1024,
            available: 6 * 1024 * 1024 * 1024,
            swap_total: 0,
            swap_used: 0,
        },
        disks: vec![DiskMetrics {
            mount: "/".into(),
            filesystem: "ext4".into(),
            total: 100_000,
            used: 40_000,
            available: 60_000,
        }],
        network: vec![NetworkMetrics {
            interface: "eth0".into(),
            rx_bytes: 1000,
            tx_bytes: 2000,
            rx_errors: 0,
            tx_errors: 0,
            rx_bytes_per_sec: Some(10.0),
            tx_bytes_per_sec: Some(20.0),
        }],
        uptime_secs: 3600,
    }
}

/// Serve the API on an ephemeral localhost port; returns its base address.
async fn spawn_server(state: Arc<ApiState>) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, api::router(state)).await.unwrap();
    });
    addr
}

mod rest {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn call(
        state: &Arc<ApiState>,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let request = match body {
            Some(json) => request
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json.to_string())),
            None => request.body(Body::empty()),
        }
        .unwrap();
        let response = api::router(Arc::clone(state))
            .oneshot(request)
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, json)
    }

    #[tokio::test]
    async fn rejects_requests_without_token() {
        let (state, _ws) = test_state();
        let (status, body) = call(&state, "GET", "/v1/status", None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(body["error"].as_str().unwrap().contains("token"));

        let (status, _) = call(&state, "GET", "/v1/status", Some("wrong"), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn status_and_app_lifecycle() {
        let (state, _ws) = test_state();
        install_fake_app(&state, "demo");

        let (status, body) = call(&state, "GET", "/v1/status", Some(TOKEN), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["version"], asc_daemon::VERSION);
        assert_eq!(body["apps_total"], 1);

        let (status, body) = call(&state, "GET", "/v1/apps", Some(TOKEN), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["apps"][0]["id"], "demo");
        assert_eq!(body["apps"][0]["state"], "stopped");

        let (status, body) = call(&state, "GET", "/v1/apps/demo", Some(TOKEN), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["app"]["kind"], "process");

        let (status, body) = call(&state, "GET", "/v1/apps/ghost", Some(TOKEN), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"].as_str().unwrap().contains("ghost"));

        let (status, _) = call(&state, "DELETE", "/v1/apps/demo", Some(TOKEN), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = call(&state, "GET", "/v1/apps/demo", Some(TOKEN), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn metrics_snapshot_and_history() {
        let (state, _ws) = test_state();

        // No samples yet — the daemon just started.
        let (status, body) = call(&state, "GET", "/v1/metrics", Some(TOKEN), None).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body["error"].as_str().unwrap().contains("no metrics"));

        state.monitor.push(fake_metrics(100));
        state.monitor.push(fake_metrics(110));

        let (status, body) = call(&state, "GET", "/v1/metrics", Some(TOKEN), None).await;
        assert_eq!(status, StatusCode::OK);
        let m = &body["metrics"];
        assert_eq!(m["timestamp"], 110);
        assert_eq!(m["cpu_usage_percent"], 12.5);
        assert_eq!(m["cpu_cores"], 4);
        assert_eq!(m["mem_total"], 8_u64 * 1024 * 1024 * 1024);
        assert_eq!(m["disks"][0]["mount"], "/");
        assert_eq!(m["network"][0]["interface"], "eth0");
        assert_eq!(m["network"][0]["rx_bytes_per_sec"], 10.0);

        // History honours the limit and returns oldest-first.
        let (status, body) = call(&state, "GET", "/v1/metrics/history", Some(TOKEN), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["samples"].as_array().unwrap().len(), 2);
        assert_eq!(body["samples"][0]["timestamp"], 100);

        let (_, body) = call(
            &state,
            "GET",
            "/v1/metrics/history?limit=1",
            Some(TOKEN),
            None,
        )
        .await;
        assert_eq!(body["samples"].as_array().unwrap().len(), 1);
        assert_eq!(body["samples"][0]["timestamp"], 110);
    }

    #[tokio::test]
    async fn console_token_flow() {
        let (state, _ws) = test_state();
        install_fake_app(&state, "demo");

        // Unknown app → no token.
        let (status, _) = call(
            &state,
            "POST",
            "/v1/apps/ghost/console-token",
            Some(TOKEN),
            Some(serde_json::json!({ "session": "logs" })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // Invalid session type.
        let (status, _) = call(
            &state,
            "POST",
            "/v1/apps/demo/console-token",
            Some(TOKEN),
            Some(serde_json::json!({ "session": "shell" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, body) = call(
            &state,
            "POST",
            "/v1/apps/demo/console-token",
            Some(TOKEN),
            Some(serde_json::json!({ "session": "attach" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let token = body["token"].as_str().unwrap();
        assert_eq!(token.len(), 64);
        assert!(body["expires_at"].as_i64().unwrap() > 0);

        // The token is single-use and bound to the app.
        let grant = state.console_tokens.consume(token).unwrap();
        assert_eq!(grant.app_id, "demo");
        assert!(state.console_tokens.consume(token).is_none());
    }
}

mod grpc {
    use super::*;
    use pb::app_service_client::AppServiceClient;
    use pb::daemon_service_client::DaemonServiceClient;
    use tonic::metadata::MetadataValue;
    use tonic::transport::Channel;

    async fn channel(addr: std::net::SocketAddr) -> Channel {
        Channel::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect()
            .await
            .unwrap()
    }

    fn with_auth<T>(mut request: tonic::Request<T>) -> tonic::Request<T> {
        let value: MetadataValue<_> = format!("Bearer {TOKEN}").parse().unwrap();
        request.metadata_mut().insert("authorization", value);
        request
    }

    #[tokio::test]
    async fn grpc_status_and_apps() {
        let (state, _ws) = test_state();
        install_fake_app(&state, "demo");
        let addr = spawn_server(state).await;

        let mut daemon = DaemonServiceClient::new(channel(addr).await);
        let status = daemon
            .get_status(with_auth(tonic::Request::new(pb::GetStatusRequest {})))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(status.version, asc_daemon::VERSION);
        assert_eq!(status.apps_total, 1);

        let mut apps = AppServiceClient::new(channel(addr).await);
        let list = apps
            .list_apps(with_auth(tonic::Request::new(pb::ListAppsRequest {})))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(list.apps.len(), 1);
        assert_eq!(list.apps[0].id, "demo");
        assert_eq!(list.apps[0].state, pb::AppState::Stopped as i32);

        let err = apps
            .get_app(with_auth(tonic::Request::new(pb::GetAppRequest {
                id: "ghost".into(),
            })))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);

        let issued = apps
            .issue_console_token(with_auth(tonic::Request::new(
                pb::IssueConsoleTokenRequest {
                    app_id: "demo".into(),
                    session: pb::ConsoleSessionType::Logs as i32,
                },
            )))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(issued.token.len(), 64);
    }

    #[tokio::test]
    async fn grpc_metrics() {
        use pb::monitor_service_client::MonitorServiceClient;

        let (state, _ws) = test_state();
        let monitor = std::sync::Arc::clone(&state.monitor);
        let addr = spawn_server(state).await;
        let mut client = MonitorServiceClient::new(channel(addr).await);

        // Empty buffer → UNAVAILABLE.
        let err = client
            .get_system_metrics(with_auth(tonic::Request::new(
                pb::GetSystemMetricsRequest {},
            )))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unavailable);

        monitor.push(fake_metrics(42));
        let metrics = client
            .get_system_metrics(with_auth(tonic::Request::new(
                pb::GetSystemMetricsRequest {},
            )))
            .await
            .unwrap()
            .into_inner()
            .metrics
            .unwrap();
        assert_eq!(metrics.timestamp, 42);
        assert_eq!(metrics.cpu_usage_percent, Some(12.5));
        assert_eq!(metrics.cpu_cores, 4);
        assert_eq!(metrics.disks[0].mount, "/");

        let history = client
            .get_metrics_history(with_auth(tonic::Request::new(
                pb::GetMetricsHistoryRequest { limit: 0 },
            )))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(history.samples.len(), 1);
    }

    #[tokio::test]
    async fn grpc_rejects_missing_token() {
        let (state, _ws) = test_state();
        let addr = spawn_server(state).await;
        let mut daemon = DaemonServiceClient::new(channel(addr).await);
        let err = daemon
            .get_status(tonic::Request::new(pb::GetStatusRequest {}))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
    }
}
