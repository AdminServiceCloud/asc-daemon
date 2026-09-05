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
            uuid: None,
            name: id.into(),
            custom_name: None,
            owner: Owner {
                uid: 0,
                name: "root".into(),
            },
            version: Some("v1.0.0".into()),
            source: Some("test:local".into()),
            branch: None,
            package: None,
            desired_state: DesiredState::Stopped,
            quota: None,
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
    use asc_daemon::daemon::monitor::GpuMetrics;
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
        gpus: vec![GpuMetrics {
            index: 0,
            vendor: "nvidia".into(),
            name: "NVIDIA GeForce RTX 4090".into(),
            utilization_percent: Some(37.0),
            memory_total: 24 * 1024 * 1024 * 1024,
            memory_used: 2 * 1024 * 1024 * 1024,
            temperature_c: Some(52.0),
            power_watts: Some(121.45),
        }],
        disk_io: vec![DiskIoMetrics {
            device: "sda".into(),
            read_bytes: 500_000,
            write_bytes: 300_000,
            read_bytes_per_sec: Some(1_000.0),
            write_bytes_per_sec: Some(500.0),
            io_ms: 42,
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
    use asc_daemon::daemon::api::console::SessionType;
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
        // DMN-076: always present so a caller can tell "no capabilities" from
        // "daemon predates this field"; DMN-083/084/087 are the first to ship.
        let capabilities: Vec<&str> = body["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            capabilities,
            vec!["sources", "credentials", "ssh-credentials"]
        );

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
    async fn app_disk_reports_directory_sizes() {
        let (state, _ws) = test_state();
        install_fake_app(&state, "demo");
        // Simulate an installed repository checkout so the breakdown is
        // non-zero; the manifest content is irrelevant here (invalid YAML
        // just means image/volume figures degrade to absent, not an error).
        let app_dir = state.config.daemon.apps_dir.join("demo");
        std::fs::create_dir_all(app_dir.join("repository")).unwrap();
        std::fs::write(app_dir.join("repository/asc.yaml"), [0u8; 128]).unwrap();
        std::fs::create_dir_all(app_dir.join("data")).unwrap();
        std::fs::write(app_dir.join("data/save.dat"), [0u8; 256]).unwrap();

        let (status, body) = call(&state, "GET", "/v1/apps/demo/disk", Some(TOKEN), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["repository_bytes"], 128);
        assert_eq!(body["data_bytes"], 256);
        assert!(body["app_dir_bytes"].as_u64().unwrap() >= 128 + 256);
        assert!(body["quota_bytes"].is_null());
        assert!(body["image_bytes"].is_null());
        assert!(body["volumes"].as_array().unwrap().is_empty());

        let (status, body) = call(&state, "GET", "/v1/apps/ghost/disk", Some(TOKEN), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"].as_str().unwrap().contains("ghost"));
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
        assert_eq!(m["gpus"][0]["vendor"], "nvidia");
        assert_eq!(m["gpus"][0]["utilization_percent"], 37.0);
        assert_eq!(m["gpus"][0]["memory_used"], 2_u64 * 1024 * 1024 * 1024);
        assert_eq!(m["disk_io"][0]["device"], "sda");
        assert_eq!(m["disk_io"][0]["read_bytes_per_sec"], 1_000.0);

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
    async fn network_interfaces_rest() {
        let (state, _ws) = test_state();
        let (status, body) = call(&state, "GET", "/v1/network/interfaces", Some(TOKEN), None).await;
        assert_eq!(status, StatusCode::OK);
        let interfaces = body["interfaces"].as_array().unwrap();
        assert!(interfaces.iter().any(|i| i["is_loopback"] == true));
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

        // DMN-082: "exec" is now a valid session type, and its command
        // round-trips into the issued grant.
        let (status, body) = call(
            &state,
            "POST",
            "/v1/apps/demo/console-token",
            Some(TOKEN),
            Some(serde_json::json!({ "session": "exec", "command": ["ls", "-la"] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let exec_token = body["token"].as_str().unwrap().to_string();
        let exec_grant = state.console_tokens.consume(&exec_token).unwrap();
        assert_eq!(exec_grant.session, SessionType::Exec);
        assert_eq!(exec_grant.command, vec!["ls", "-la"]);

        // No command → an empty probe list, not a missing-field error.
        let (status, body) = call(
            &state,
            "POST",
            "/v1/apps/demo/console-token",
            Some(TOKEN),
            Some(serde_json::json!({ "session": "exec" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let bare_token = body["token"].as_str().unwrap();
        assert!(
            state
                .console_tokens
                .consume(bare_token)
                .unwrap()
                .command
                .is_empty()
        );
    }
}

/// DMN-070: the file API's REST content route, over the same bearer-token
/// TCP transport the rest of this file exercises.
mod files {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn raw(
        state: &Arc<ApiState>,
        method: &str,
        uri: &str,
        token: &str,
        body: Vec<u8>,
    ) -> (StatusCode, Vec<u8>) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(body))
            .unwrap();
        let response = api::router(Arc::clone(state))
            .oneshot(request)
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, bytes.to_vec())
    }

    async fn json(
        state: &Arc<ApiState>,
        uri: &str,
        token: &str,
    ) -> (StatusCode, serde_json::Value) {
        let request = Request::builder()
            .method("GET")
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
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

    /// `PUT` then `GET` on `/v1/files/content` round-trip the same bytes, and
    /// a successful upload leaves no `.asc-upload-*.part` staging file behind
    /// in the target directory.
    #[tokio::test]
    async fn upload_then_download_round_trips_bytes() {
        let (state, ws) = test_state();
        let dir = ws.path().join("uploads");
        std::fs::create_dir_all(&dir).unwrap();
        let dir_str = dir.display().to_string();

        let (status, _) = raw(
            &state,
            "PUT",
            &format!("/v1/files/content?path={dir_str}&name=hello.txt"),
            TOKEN,
            b"hello world".to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            entries,
            vec![std::ffi::OsString::from("hello.txt")],
            "no leftover .part staging file, got {entries:?}"
        );

        let (status, bytes) = raw(
            &state,
            "GET",
            &format!("/v1/files/content?path={dir_str}/hello.txt"),
            TOKEN,
            vec![],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(bytes, b"hello world");
    }

    #[tokio::test]
    async fn uploading_over_an_existing_file_without_overwrite_is_refused() {
        let (state, ws) = test_state();
        let dir = ws.path().join("uploads");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("taken.txt"), b"existing").unwrap();
        let dir_str = dir.display().to_string();

        let (status, _) = raw(
            &state,
            "PUT",
            &format!("/v1/files/content?path={dir_str}&name=taken.txt"),
            TOKEN,
            b"new".to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(std::fs::read(dir.join("taken.txt")).unwrap(), b"existing");

        let (status, _) = raw(
            &state,
            "PUT",
            &format!("/v1/files/content?path={dir_str}&name=taken.txt&overwrite=true"),
            TOKEN,
            b"new".to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(std::fs::read(dir.join("taken.txt")).unwrap(), b"new");
    }

    /// Files are not primary-only, unlike `/v1/token/*` — a short-lived
    /// access token can list and read them like any other route.
    #[tokio::test]
    async fn an_access_token_may_list_files() {
        let (state, ws) = test_state();
        std::fs::write(ws.path().join("note.txt"), b"hi").unwrap();
        let (access, _) = state.tokens.issue_access(None, "test");

        let (status, body) = json(
            &state,
            &format!("/v1/files?path={}", ws.path().display()),
            &access,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let names: Vec<_> = body["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"note.txt"), "got: {names:?}");
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
        assert_eq!(
            status.capabilities,
            vec!["sources", "credentials", "ssh-credentials"]
        );

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
                    command: vec![],
                },
            )))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(issued.token.len(), 64);
    }

    #[tokio::test]
    async fn grpc_app_disk() {
        let (state, _ws) = test_state();
        install_fake_app(&state, "demo");
        let app_dir = state.config.daemon.apps_dir.join("demo");
        std::fs::create_dir_all(app_dir.join("repository")).unwrap();
        std::fs::write(app_dir.join("repository/asc.yaml"), [0u8; 64]).unwrap();
        let addr = spawn_server(state).await;

        let mut apps = AppServiceClient::new(channel(addr).await);
        let usage = apps
            .get_app_disk(with_auth(tonic::Request::new(pb::GetAppDiskRequest {
                id: "demo".into(),
            })))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(usage.repository_bytes, 64);
        assert!(usage.quota_bytes.is_none());

        let err = apps
            .get_app_disk(with_auth(tonic::Request::new(pb::GetAppDiskRequest {
                id: "ghost".into(),
            })))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
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
        assert_eq!(metrics.gpus[0].vendor, "nvidia");
        assert_eq!(metrics.gpus[0].temperature_c, Some(52.0));
        assert_eq!(metrics.disk_io[0].device, "sda");
        assert_eq!(metrics.disk_io[0].io_ms, 42);

        let history = client
            .get_metrics_history(with_auth(tonic::Request::new(
                pb::GetMetricsHistoryRequest { limit: 0 },
            )))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(history.samples.len(), 1);

        // The stream (DMN-072) opens with the sample already in the buffer
        // (DMN-075) so a panel paints without waiting out a sampling
        // interval, then continues as a live feed off the broadcast channel.
        let mut stream = client
            .stream_system_metrics(with_auth(tonic::Request::new(
                pb::StreamSystemMetricsRequest {},
            )))
            .await
            .unwrap()
            .into_inner();
        let buffered = stream.message().await.unwrap().unwrap();
        assert_eq!(buffered.timestamp, 42);
        monitor.push(fake_metrics(99));
        let streamed = stream.message().await.unwrap().unwrap();
        assert_eq!(streamed.timestamp, 99);

        // The machine running the test always has a loopback interface.
        let interfaces = client
            .list_network_interfaces(with_auth(tonic::Request::new(
                pb::ListNetworkInterfacesRequest {},
            )))
            .await
            .unwrap()
            .into_inner()
            .interfaces;
        assert!(interfaces.iter().any(|i| i.is_loopback));
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
