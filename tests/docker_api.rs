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
use asc_daemon::daemon::docker::{self, CreateSpec};
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

                    let (code, body) = route(method, path);
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
fn route(method: &str, raw_path: &str) -> (&'static str, String) {
    let path = raw_path.split('?').next().unwrap_or(raw_path);
    if path.contains("/containers/create") {
        return ("201 Created", r#"{"Id":"deadbeef","Warnings":[]}"#.into());
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
        return ("200 OK", r#"{"State":{"Running":true}}"#.into());
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
            ports: vec![8080],
            binds: vec!["/asc/apps/web/data/data:/data".into()],
        },
    )
    .unwrap();

    let seen = hits.lock().unwrap().clone();
    assert!(
        seen.iter().any(|h| h.contains("/containers/create")),
        "create must hit the Engine create endpoint, saw: {seen:?}"
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
