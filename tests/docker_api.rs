//! Docker Engine API integration over a mock unix socket.
//!
//! Spins up a minimal HTTP server on a temp unix socket that answers like the
//! Docker Engine, points `[docker] socket` at it, and drives the daemon's
//! control-plane operations. Proves the socket path, request routing and
//! response parsing without requiring a real Docker daemon.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;

use asc_daemon::daemon::config::DockerConfig;
use asc_daemon::daemon::docker::{self, CreateSpec, PortProtocol};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;

/// Records the request paths the mock received, for assertions.
type Hits = Arc<Mutex<Vec<String>>>;

/// Start the mock Docker Engine on `socket`, serving until the process ends.
fn spawn_mock(socket: PathBuf) -> Hits {
    let hits: Hits = Arc::new(Mutex::new(Vec::new()));
    let hits_srv = Arc::clone(&hits);
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = UnixListener::bind(&socket).unwrap();
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let hits = Arc::clone(&hits_srv);
                tokio::spawn(async move {
                    // Read the request head (up to the blank line).
                    let mut buf = Vec::new();
                    let mut byte = [0u8; 1];
                    while stream.read_exact(&mut byte).await.is_ok() {
                        buf.push(byte[0]);
                        if buf.ends_with(b"\r\n\r\n") {
                            break;
                        }
                    }
                    let head = String::from_utf8_lossy(&buf);
                    let request_line = head.lines().next().unwrap_or("").to_string();
                    let mut parts = request_line.split_whitespace();
                    let method = parts.next().unwrap_or("");
                    let path = parts.next().unwrap_or("");
                    hits.lock().unwrap().push(format!("{method} {path}"));

                    let seen = hits.lock().unwrap().clone();
                    let (code, body) = route(method, path, &seen);
                    let response = format!(
                        "HTTP/1.1 {code}\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).await.ok();
                    stream.shutdown().await.ok();
                });
            }
        });
    });
    hits
}

/// Minimal Docker Engine routing, matched loosely by path suffix so any API
/// version prefix works. Query string is ignored (e.g. `/stop?t=10`).
/// `seen` holds the previously recorded requests: creating a container 404s
/// until the image has been pulled, exercising the auto-pull retry.
fn route(method: &str, raw_path: &str, seen: &[String]) -> (&'static str, String) {
    let path = raw_path.split('?').next().unwrap_or(raw_path);
    if path.contains("/images/create") {
        return (
            "200 OK",
            r#"{"status":"Pulling from library/nginx"}"#.into(),
        );
    }
    if path.contains("/containers/create") {
        if !seen.iter().any(|h| h.contains("/images/create")) {
            return (
                "404 Not Found",
                r#"{"message":"No such image: nginx:1.27"}"#.into(),
            );
        }
        return ("201 Created", r#"{"Id":"deadbeef","Warnings":[]}"#.into());
    }
    // A named engine-side failure, for error classification tests.
    if path.contains("/containers/boom/") {
        return (
            "500 Internal Server Error",
            r#"{"message":"server exploded"}"#.into(),
        );
    }
    if path.ends_with("/start") {
        return ("204 No Content", String::new());
    }
    if path.ends_with("/stop") {
        return ("204 No Content", String::new());
    }
    if path.ends_with("/restart") {
        return ("204 No Content", String::new());
    }
    if path.contains("missing") && path.ends_with("/json") {
        return ("404 Not Found", r#"{"message":"no such container"}"#.into());
    }
    if path.ends_with("/json") {
        return (
            "200 OK",
            r#"{"State":{"Running":true},"Config":{"Env":["PATH=/usr/bin","CS2_STARTMAP=de_dust2"]},"HostConfig":{"PortBindings":{"27015/tcp":[{"HostIp":"","HostPort":"27015"}]},"NanoCpus":0,"Memory":0}}"#.into(),
        );
    }
    if method == "DELETE" {
        return ("204 No Content", String::new());
    }
    ("404 Not Found", r#"{"message":"unhandled"}"#.into())
}

fn wait_for_socket(path: &Path) {
    for _ in 0..50 {
        if path.exists() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("mock docker socket never appeared at {}", path.display());
}

fn test_cfg() -> (DockerConfig, tempfile::TempDir, Hits) {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("docker.sock");
    let hits = spawn_mock(socket.clone());
    wait_for_socket(&socket);
    (DockerConfig { socket }, dir, hits)
}

#[test]
fn lifecycle_over_engine_api() {
    let (cfg, _dir, hits) = test_cfg();

    docker::start(&cfg, "asc-demo").unwrap();
    docker::stop(&cfg, "asc-demo").unwrap();
    docker::restart(&cfg, "asc-demo").unwrap();
    assert!(docker::running(&cfg, "asc-demo").unwrap());
    // A 404 from inspect reads as "not running", never an error.
    assert!(!docker::running(&cfg, "missing").unwrap());
    docker::remove(&cfg, "asc-demo").unwrap();

    let seen = hits.lock().unwrap().clone();
    assert!(
        seen.iter()
            .any(|h| h.contains("/containers/asc-demo/start"))
    );
    assert!(seen.iter().any(|h| h.contains("/containers/asc-demo/stop")));
    assert!(
        seen.iter()
            .any(|h| h.contains("/containers/asc-demo/restart"))
    );
    assert!(seen.iter().any(|h| h.starts_with("DELETE")));
}

#[test]
fn create_sends_container_spec() {
    let (cfg, _dir, hits) = test_cfg();

    docker::create(
        &cfg,
        CreateSpec {
            name: "asc-web",
            image: "nginx:1.27",
            env: vec!["PORT=8080".into()],
            ports: vec![(8080, PortProtocol::Tcp)],
            binds: vec!["/asc/apps/web/data/data:/data".into()],
            nano_cpus: Some(1_500_000_000),
            memory_bytes: Some(512 << 20),
            command: Some("echo ready".into()),
            open_stdin: true,
            tty: true,
        },
    )
    .unwrap();

    let seen = hits.lock().unwrap().clone();
    assert!(
        seen.iter().any(|h| h.contains("/containers/create")),
        "create must hit the Engine create endpoint, saw: {seen:?}"
    );
    assert!(
        seen.iter()
            .any(|h| h.contains("/images/create") && h.contains("fromImage=nginx")),
        "a missing image must be pulled automatically, saw: {seen:?}"
    );
    assert_eq!(
        seen.iter()
            .filter(|h| h.contains("/containers/create"))
            .count(),
        2,
        "create must be retried after the pull, saw: {seen:?}"
    );
}

#[test]
fn container_applied_reads_inspect_and_tolerates_missing() {
    let (cfg, _dir, _hits) = test_cfg();

    let applied = docker::container_applied(&cfg, "asc-demo")
        .unwrap()
        .unwrap();
    assert_eq!(applied.env, ["PATH=/usr/bin", "CS2_STARTMAP=de_dust2"]);
    assert_eq!(applied.ports, ["27015/tcp"]);
    assert!(applied.binds.is_empty());
    assert_eq!((applied.nano_cpus, applied.memory), (0, 0));
    // A missing container (404) reads as None — the caller recreates it.
    assert!(
        docker::container_applied(&cfg, "missing")
            .unwrap()
            .is_none()
    );
}

#[test]
fn engine_errors_are_not_reported_as_unreachable() {
    let (cfg, _dir, _hits) = test_cfg();

    let err = docker::start(&cfg, "boom").unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("server exploded"),
        "the Engine's own message must survive, got: {msg}"
    );
    assert!(
        !msg.contains("cannot reach Docker"),
        "an Engine response is not a connectivity failure, got: {msg}"
    );
}

/// DMN-017/030: a stopped container whose configuration (env, ports, quota
/// override…) drifted from settings.json is recreated on refresh; a
/// matching configuration is left alone.
#[test]
fn settings_drift_recreates_the_container() {
    use asc_daemon::daemon::apps::meta::{Owner, Runtime};
    use asc_daemon::daemon::apps::{AppMeta, AppStore, DesiredState};
    use asc_daemon::daemon::pkg::refresh;

    let (docker_cfg, _dir, hits) = test_cfg();
    let apps = tempfile::tempdir().unwrap();
    let mut config = asc_daemon::daemon::config::Config::default();
    config.daemon.apps_dir = apps.path().to_path_buf();
    config.docker = docker_cfg;

    let store = AppStore::new(apps.path().to_path_buf());
    let mut meta = AppMeta {
        id: "web".into(),
        name: "web".into(),
        custom_name: None,
        owner: Owner {
            uid: 1000,
            name: "user".into(),
        },
        version: None,
        source: None,
        package: None,
        desired_state: DesiredState::Stopped,
        quota: None,
        runtime: Runtime::Docker {
            container: "asc-web".into(),
        },
    };
    store.save(&meta).unwrap();
    let app_dir = store.app_dir("web").unwrap();
    std::fs::create_dir_all(app_dir.join("repository")).unwrap();
    std::fs::create_dir_all(app_dir.join("config")).unwrap();
    std::fs::write(
        app_dir.join("repository/asc.yaml"),
        "name: web\nversion: '1'\ntype: docker\nsettings: ./asc.settings.yaml\n\
         runtime:\n  image: nginx:1.27\n",
    )
    .unwrap();
    std::fs::write(
        app_dir.join("repository/asc.settings.yaml"),
        "settings:\n  - { key: map, type: enum, values: [de_dust2, de_mirage], \
         default: de_dust2, env: CS2_STARTMAP }\n  - { key: game_port, type: ports, \
         default: [27015] }\n",
    )
    .unwrap();
    let deletes = || {
        hits.lock()
            .unwrap()
            .iter()
            .filter(|h| h.starts_with("DELETE"))
            .count()
    };

    // Everything matches what the mock inspect reports (CS2_STARTMAP=
    // de_dust2, port 27015/tcp published, no quota): nothing to do.
    std::fs::write(
        app_dir.join("config/settings.json"),
        r#"{"map":"de_dust2","game_port":[27015]}"#,
    )
    .unwrap();
    assert!(!refresh::apply_settings(&config, &mut meta, &app_dir).unwrap());
    assert_eq!(deletes(), 0, "matching config must not recreate");

    // A changed map drifts from the container env: remove + create.
    std::fs::write(
        app_dir.join("config/settings.json"),
        r#"{"map":"de_mirage","game_port":[27015]}"#,
    )
    .unwrap();
    assert!(refresh::apply_settings(&config, &mut meta, &app_dir).unwrap());
    assert_eq!(deletes(), 1, "drifted env must recreate the container");
    let seen = hits.lock().unwrap().clone();
    assert!(
        seen.iter().any(|h| h.contains("/containers/create")),
        "drifted env must create a fresh container, saw: {seen:?}"
    );

    // Changed published ports drift too (DMN-030).
    std::fs::write(
        app_dir.join("config/settings.json"),
        r#"{"map":"de_dust2","game_port":[27016]}"#,
    )
    .unwrap();
    assert!(refresh::apply_settings(&config, &mut meta, &app_dir).unwrap());
    assert_eq!(deletes(), 2, "changed ports must recreate the container");

    // A quota override drifts as well: the mock reports no limits.
    std::fs::write(
        app_dir.join("config/settings.json"),
        r#"{"map":"de_dust2","game_port":[27015],"$quota":{"max_ram":"1G"}}"#,
    )
    .unwrap();
    assert!(refresh::apply_settings(&config, &mut meta, &app_dir).unwrap());
    assert_eq!(deletes(), 3, "quota override must recreate the container");
    assert_eq!(
        meta.quota.as_ref().and_then(|q| q.ram_bytes),
        Some(1 << 30),
        "meta.quota must reflect the applied override"
    );
}

#[test]
fn missing_socket_is_a_friendly_error() {
    let cfg = DockerConfig {
        socket: PathBuf::from("/nonexistent/docker.sock"),
    };
    let err = docker::start(&cfg, "asc-demo").unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("/nonexistent/docker.sock"),
        "error should name the socket path, got: {msg}"
    );
}
