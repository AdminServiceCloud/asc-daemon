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
use asc_daemon::daemon::docker::{self, CreateSpec, PortProtocol, PublishedPort};
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
                    // Registry credentials travel as a base64 X-Registry-Auth
                    // header (DMN-046); record it so tests can assert on it.
                    if let Some(value) = head
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("x-registry-auth:"))
                        .and_then(|l| l.split_once(':'))
                        .map(|(_, v)| v.trim().to_string())
                    {
                        hits.lock().unwrap().push(format!("AUTH {value}"));
                    }
                    // The JSON body of a request that has one (create), so
                    // tests can assert on the spec the daemon sends.
                    if let Some(len) = head
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                        .and_then(|l| l.split_once(':'))
                        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
                        .filter(|len| *len > 0)
                    {
                        let mut body = vec![0u8; len];
                        if stream.read_exact(&mut body).await.is_ok() {
                            let body = String::from_utf8_lossy(&body).to_string();
                            hits.lock().unwrap().push(format!("BODY {body}"));
                        }
                    }

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
    // DMN-104/DMN-105 host inventory & cleanup — matched before the
    // `/containers/create` and generic `/json`/`DELETE` arms below, since
    // `/images/json` would otherwise fall through to the container-inspect
    // fallback and `DELETE /images/<id>` needs its own body shape
    // (`remove_image` decodes a JSON array, unlike `remove_container`).
    if path.ends_with("/images/json") {
        return ("200 OK", IMAGE_LIST.into());
    }
    if path.ends_with("/volumes") {
        return ("200 OK", VOLUME_LIST.into());
    }
    if path.ends_with("/networks") {
        return ("200 OK", NETWORK_LIST.into());
    }
    if path.ends_with("/system/df") {
        return ("200 OK", DISK_USAGE.into());
    }
    if path.contains("/build/prune") {
        return (
            "200 OK",
            r#"{"CachesDeleted":["cache1"],"SpaceReclaimed":1048576}"#.into(),
        );
    }
    if method == "DELETE" && path.contains("/images/") {
        return ("200 OK", "[]".into());
    }
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
    // An image the host does not have yet, so ensure_pulled goes on to pull.
    if path.contains("/images/") && path.contains("private") && path.ends_with("/json") {
        return ("404 Not Found", r#"{"message":"no such image"}"#.into());
    }
    // `docker ps`: the container *list*, not one container's inspect — it
    // has to be matched before the generic `/json` arm below, which both
    // paths would otherwise hit.
    if path.ends_with("/containers/json") {
        return ("200 OK", CONTAINER_LIST.into());
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

/// Fixture for `GET /containers/json` (DMN-102): one container that belongs
/// to an ASC app, one raised by Compose, one stopped and unpublished.
const CONTAINER_LIST: &str = r#"[
  {
    "Id": "aaaa000000000000000000000000000000000000000000000000000000000001",
    "Names": ["/asc-helloworld"],
    "Image": "ghcr.io/acme/hello:1.0",
    "ImageID": "sha256:abc",
    "State": "running",
    "Status": "Up 3 hours",
    "Created": 1700000000,
    "Ports": [{"IP": "0.0.0.0", "PrivatePort": 3000, "PublicPort": 8080, "Type": "tcp"}],
    "Labels": {"maintainer": "acme"},
    "NetworkSettings": {"Networks": {"bridge": {}}}
  },
  {
    "Id": "bbbb000000000000000000000000000000000000000000000000000000000002",
    "Names": ["/shop-db-1"],
    "Image": "postgres:16",
    "ImageID": "sha256:def",
    "State": "running",
    "Status": "Up 2 days",
    "Created": 1700000100,
    "Ports": [{"PrivatePort": 5432, "Type": "tcp"}],
    "Labels": {
      "com.docker.compose.project": "shop",
      "com.docker.compose.service": "db"
    },
    "NetworkSettings": {"Networks": {"shop_default": {}, "bridge": {}}}
  },
  {
    "Id": "cccc000000000000000000000000000000000000000000000000000000000003",
    "Names": ["/manual"],
    "Image": "alpine:3.20",
    "ImageID": "sha256:ghi",
    "State": "exited",
    "Status": "Exited (0) 5 minutes ago",
    "Created": 1700000200
  }
]"#;

/// Fixture for `GET /images/json` (DMN-104): one image an installed app
/// would run, one dangling (untagged) image.
const IMAGE_LIST: &str = r#"[
  {
    "Id": "sha256:aaaa111100000000000000000000000000000000000000000000000000000",
    "ParentId": "",
    "RepoTags": ["ghcr.io/acme/hello:1.0"],
    "RepoDigests": [],
    "Created": 1700000000,
    "Size": 104857600,
    "SharedSize": -1,
    "Labels": {},
    "Containers": -1
  },
  {
    "Id": "sha256:bbbb222200000000000000000000000000000000000000000000000000000",
    "ParentId": "",
    "RepoTags": ["<none>:<none>"],
    "RepoDigests": [],
    "Created": 1699999000,
    "Size": 52428800,
    "SharedSize": -1,
    "Labels": {},
    "Containers": -1
  }
]"#;

/// Fixture for `GET /volumes` (DMN-104): one volume an installed app would
/// declare, one unrelated.
const VOLUME_LIST: &str = r#"{
  "Volumes": [
    {
      "Name": "shared-data",
      "Driver": "local",
      "Mountpoint": "/var/lib/docker/volumes/shared-data/_data",
      "Labels": {},
      "Options": {},
      "Scope": "local",
      "UsageData": {"Size": 2048, "RefCount": 1}
    },
    {
      "Name": "orphan",
      "Driver": "local",
      "Mountpoint": "/var/lib/docker/volumes/orphan/_data",
      "Labels": {},
      "Options": {},
      "Scope": "local",
      "UsageData": {"Size": 4096, "RefCount": 0}
    }
  ]
}"#;

/// Fixture for `GET /networks` (DMN-104).
const NETWORK_LIST: &str = r#"[
  {"Id": "net1", "Name": "bridge", "Driver": "bridge", "Scope": "local", "Internal": false},
  {"Id": "net2", "Name": "shop_default", "Driver": "bridge", "Scope": "local", "Internal": false}
]"#;

/// Fixture for `GET /system/df` (DMN-104).
const DISK_USAGE: &str = r#"{
  "ImageUsage": {"ActiveCount": 1, "TotalCount": 2, "Reclaimable": 52428800, "TotalSize": 157286400},
  "ContainerUsage": {"ActiveCount": 2, "TotalCount": 3, "Reclaimable": 0, "TotalSize": 0},
  "VolumeUsage": {"ActiveCount": 1, "TotalCount": 2, "Reclaimable": 4096, "TotalSize": 6144},
  "BuildCacheUsage": {"ActiveCount": 0, "TotalCount": 1, "Reclaimable": 1048576, "TotalSize": 1048576}
}"#;

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

/// DMN-046: a registry credential must reach the Engine as X-Registry-Auth,
/// which is what lets it pull a private image on the daemon's behalf.
#[test]
fn pull_sends_registry_credentials() {
    let (cfg, _dir, hits) = test_cfg();

    docker::ensure_pulled(
        &cfg,
        "ghcr.io/org/private:1.0",
        Some(&docker::RegistryAuth {
            username: "statebyte".into(),
            token: "ghp_secret".into(),
        }),
        None,
    )
    .unwrap();

    let seen = hits.lock().unwrap().clone();
    assert!(
        seen.iter().any(|h| h.contains("/images/create")),
        "expected a pull, got {seen:?}"
    );
    let auth = seen
        .iter()
        .find_map(|h| h.strip_prefix("AUTH "))
        .expect("pull must carry an X-Registry-Auth header");

    // The header is base64 of the credentials JSON.
    let decoded =
        String::from_utf8(base64_decode(auth).expect("X-Registry-Auth must be valid base64"))
            .unwrap();
    assert!(decoded.contains("statebyte"), "decoded: {decoded}");
    assert!(decoded.contains("ghp_secret"), "decoded: {decoded}");
}

/// Minimal standard-alphabet base64 decoder — the test needs to read one
/// header and the crate has no base64 dependency of its own.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut bits = 0;
    for byte in input
        .bytes()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'=')
    {
        let value = ALPHABET.iter().position(|c| *c == byte)? as u32;
        acc = (acc << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[test]
fn create_sends_container_spec() {
    let (cfg, _dir, hits) = test_cfg();

    docker::create(
        &cfg,
        CreateSpec {
            name: "asc-web",
            image: "nginx:1.27",
            env: vec!["PORT=3000".into()],
            ports: vec![PublishedPort {
                host: 8080,
                container: 3000,
                protocol: PortProtocol::Tcp,
            }],
            binds: vec!["/asc/apps/web/data/data:/data".into()],
            nano_cpus: Some(1_500_000_000),
            memory_bytes: Some(512 << 20),
            command: Some("echo ready".into()),
            open_stdin: true,
            tty: true,
            registry_auth: None,
            labels: std::collections::HashMap::new(),
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
    // The Engine names the port by its container side and carries the host
    // side in the binding — the user's 8080 reaches the app's 3000.
    let body = seen
        .iter()
        .find(|h| h.starts_with("BODY ") && h.contains("PortBindings"))
        .unwrap_or_else(|| panic!("create must send a container spec, saw: {seen:?}"));
    assert!(
        body.contains(r#""3000/tcp":[{"HostPort":"8080"}]"#)
            || body.contains(r#""3000/tcp":[{"HostIp":null,"HostPort":"8080"}]"#),
        "host 8080 must publish onto container 3000, got: {body}"
    );
    assert!(
        body.contains(r#""ExposedPorts""#) && body.contains(r#""3000/tcp""#),
        "the exposed port is the container side, got: {body}"
    );
}

#[test]
fn container_applied_reads_inspect_and_tolerates_missing() {
    let (cfg, _dir, _hits) = test_cfg();

    let applied = docker::container_applied(&cfg, "asc-demo")
        .unwrap()
        .unwrap();
    assert_eq!(applied.env, ["PATH=/usr/bin", "CS2_STARTMAP=de_dust2"]);
    // Normalized as host:container/transport, so a host port changed under
    // an unchanged container port still reads as drift.
    assert_eq!(applied.ports, ["27015:27015/tcp"]);
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
        uuid: None,
        name: "web".into(),
        custom_name: None,
        owner: Owner {
            uid: 1000,
            name: "user".into(),
        },
        version: None,
        source: None,
        branch: None,
        repo_path: None,
        package: None,
        install_method: None,
        desired_state: DesiredState::Stopped,
        quota: None,
        runtime: Runtime::Docker {
            container: "asc-web".into(),
            image_source: None,
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

    // DMN-052: the package now fixes the container side, so only the *host*
    // port moves. The Engine's own key ("27015/tcp") is identical before and
    // after — the drift check must compare the host side too, or the app
    // would keep answering on the port the user just left behind.
    std::fs::write(
        app_dir.join("repository/asc.settings.yaml"),
        "settings:\n  - { key: map, type: enum, values: [de_dust2, de_mirage], \
         default: de_dust2, env: CS2_STARTMAP }\n  - { key: game_port, type: ports, \
         default: [27015], container: 27015 }\n",
    )
    .unwrap();
    std::fs::write(
        app_dir.join("config/settings.json"),
        r#"{"map":"de_dust2","game_port":[27016]}"#,
    )
    .unwrap();
    assert!(refresh::apply_settings(&config, &mut meta, &app_dir).unwrap());
    assert_eq!(
        deletes(),
        4,
        "a changed host port must recreate the container"
    );

    // The same mapping as the mock reports (host 27015 → container 27015) is
    // not drift.
    std::fs::write(
        app_dir.join("config/settings.json"),
        r#"{"map":"de_dust2","game_port":[27015]}"#,
    )
    .unwrap();
    assert!(!refresh::apply_settings(&config, &mut meta, &app_dir).unwrap());
    assert_eq!(deletes(), 4, "an unchanged mapping must not recreate");
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
/// The Engine's container summaries reach the daemon's own shape intact:
/// names lose the historic leading slash, Compose labels are lifted into
/// their own fields, an exposed-only port keeps no host side, and a
/// container with no networks reports none rather than failing to parse.
#[test]
fn container_list_maps_engine_summaries() {
    let (cfg, _dir, _hits) = test_cfg();

    let containers = docker::list_containers(&cfg, true, false).unwrap();
    assert_eq!(containers.len(), 3);

    let app = &containers[0];
    assert_eq!(app.names, vec!["asc-helloworld".to_string()]);
    assert_eq!(app.image, "ghcr.io/acme/hello:1.0");
    assert_eq!(app.state, "running");
    assert_eq!(app.status, "Up 3 hours");
    assert_eq!(app.created, 1_700_000_000);
    assert_eq!(app.ports.len(), 1);
    assert_eq!(app.ports[0].private, 3000);
    assert_eq!(app.ports[0].public, Some(8080));
    assert_eq!(app.ports[0].protocol, "tcp");
    assert_eq!(app.ports[0].ip, "0.0.0.0");
    assert_eq!(app.networks, vec!["bridge".to_string()]);
    // The list itself never claims ownership — that is resolved against the
    // app store one layer up, so nothing here may guess from the name.
    assert_eq!(app.compose_project, None);

    let compose = &containers[1];
    assert_eq!(compose.compose_project.as_deref(), Some("shop"));
    assert_eq!(compose.compose_service.as_deref(), Some("db"));
    // Exposed but not published: no host side at all, not a zero.
    assert_eq!(compose.ports[0].public, None);
    // Networks come back sorted, so a caller can compare them directly.
    assert_eq!(
        compose.networks,
        vec!["bridge".to_string(), "shop_default".to_string()]
    );

    let manual = &containers[2];
    assert_eq!(manual.names, vec!["manual".to_string()]);
    assert_eq!(manual.state, "exited");
    assert!(manual.ports.is_empty());
    assert!(manual.networks.is_empty());
    assert!(manual.labels.is_empty());
}

/// Sizes are opt-in: `size=1` makes the Engine walk every container's
/// writable layer, so a plain listing must never ask for it.
#[test]
fn container_list_asks_for_sizes_only_when_requested() {
    let (cfg, _dir, hits) = test_cfg();

    docker::list_containers(&cfg, false, false).unwrap();
    let plain = hits.lock().unwrap().clone();
    let plain = plain
        .iter()
        .find(|h| h.contains("/containers/json"))
        .expect("no container list request recorded");
    assert!(
        plain.contains("size=false") || !plain.contains("size=true"),
        "a plain listing must not request sizes: {plain}"
    );

    let (cfg, _dir, hits) = test_cfg();
    docker::list_containers(&cfg, false, true).unwrap();
    let sized = hits.lock().unwrap().clone();
    let sized = sized
        .iter()
        .find(|h| h.contains("/containers/json"))
        .expect("no container list request recorded");
    assert!(
        sized.contains("size=true"),
        "an explicit size request must reach the Engine: {sized}"
    );
}

/// A container the Engine reports without sizes must not read as "0 bytes"
/// just because the caller asked for them.
#[test]
fn container_list_leaves_missing_sizes_unset() {
    let (cfg, _dir, _hits) = test_cfg();

    let containers = docker::list_containers(&cfg, true, true).unwrap();
    assert!(containers.iter().all(|c| c.size_rw.is_none()));
    assert!(containers.iter().all(|c| c.size_root_fs.is_none()));
}

/// DMN-104: `GET /images/json`, `GET /volumes`, `GET /networks` and
/// `GET /system/df` all parse into the daemon's own forms.
#[test]
fn host_inventory_parses_images_volumes_networks_and_disk_usage() {
    let (cfg, _dir, _hits) = test_cfg();

    let images = docker::list_images(&cfg).unwrap();
    assert_eq!(images.len(), 2);
    assert_eq!(images[0].tags, vec!["ghcr.io/acme/hello:1.0".to_string()]);
    assert!(!images[0].dangling);
    assert!(images[1].tags.is_empty());
    assert!(images[1].dangling);

    let volumes = docker::list_volumes(&cfg).unwrap();
    assert_eq!(volumes.len(), 2);
    assert_eq!(volumes[0].name, "shared-data");
    assert_eq!(volumes[0].size_bytes, Some(2048));
    assert_eq!(volumes[0].ref_count, Some(1));

    let networks = docker::list_networks(&cfg).unwrap();
    assert_eq!(networks.len(), 2);
    assert_eq!(networks[0].name, "bridge");

    let usage = docker::disk_usage(&cfg).unwrap();
    assert_eq!(usage.images.total_count, 2);
    assert_eq!(usage.images.reclaimable_bytes, 52428800);
    assert_eq!(usage.volumes.active_count, 1);
    assert_eq!(usage.build_cache.reclaimable_bytes, 1048576);
}

/// Installs a minimal docker-runtime app for DMN-105's protected-set scan
/// (`AppManager::list` + `pkg::docker_footprint`) to find — no settings, just
/// the manifest's `runtime.image`.
fn install_docker_app(config: &asc_daemon::daemon::config::Config, id: &str, image: &str) {
    use asc_daemon::daemon::apps::AppStore;
    use asc_daemon::daemon::apps::meta::{AppMeta, DesiredState, Owner, Runtime};

    let store = AppStore::new(config.daemon.apps_dir.clone());
    store
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
            repo_path: None,
            package: None,
            install_method: None,
            desired_state: DesiredState::Stopped,
            quota: None,
            runtime: Runtime::Docker {
                container: format!("asc-{id}"),
                image_source: None,
            },
        })
        .unwrap();
    let app_dir = store.app_dir(id).unwrap();
    std::fs::create_dir_all(app_dir.join("repository")).unwrap();
    std::fs::create_dir_all(app_dir.join("config")).unwrap();
    std::fs::write(
        app_dir.join("repository/asc.yaml"),
        format!("name: {id}\nversion: '1'\ntype: docker\nruntime:\n  image: {image}\n"),
    )
    .unwrap();
}

/// A test-only root context, mirroring the daemon's own `api_context()`
/// (private to `daemon::api`, so the test builds its own).
fn root_ctx() -> asc_daemon::daemon::apps::UserContext {
    asc_daemon::daemon::apps::UserContext {
        uid: 0,
        name: "root".into(),
        is_root: true,
    }
}

/// DMN-105: an image an installed app runs — running or not — must never be
/// removed, dry_run or not, and the dangling one is reported as removable.
#[tokio::test]
async fn prune_images_dry_run_protects_installed_apps_image() {
    use asc_daemon::daemon::api::{ApiState, PruneTarget};

    let (docker_cfg, dir, hits) = test_cfg();
    let mut config = asc_daemon::daemon::config::Config::default();
    config.daemon.apps_dir = dir.path().join("apps");
    config.docker = docker_cfg;
    install_docker_app(&config, "web", "ghcr.io/acme/hello:1.0");

    let state = ApiState::new(config, "test-token".into());

    let images = state.list_images(root_ctx()).await.unwrap();
    let tagged = images
        .iter()
        .find(|i| i.tags.contains(&"ghcr.io/acme/hello:1.0".to_string()))
        .unwrap();
    assert!(
        tagged.asc_protected,
        "an installed app's image must be protected"
    );
    assert!(tagged.protected_reason.as_deref().unwrap().contains("web"));
    let dangling = images.iter().find(|i| i.dangling).unwrap();
    assert!(!dangling.asc_protected);

    let plan = state
        .prune_docker(root_ctx(), PruneTarget::Images, true, false)
        .await
        .unwrap();
    assert!(
        !plan.removed.contains(&"ghcr.io/acme/hello:1.0".to_string()),
        "the protected image must never appear in removed, got {:?}",
        plan.removed
    );
    assert!(
        plan.skipped
            .iter()
            .any(|s| s.name == "ghcr.io/acme/hello:1.0"),
        "the protected image must be explained in skipped, got {:?}",
        plan.skipped.iter().map(|s| &s.name).collect::<Vec<_>>()
    );

    let seen = hits.lock().unwrap().clone();
    assert!(
        !seen.iter().any(|h| h.starts_with("DELETE")),
        "dry_run must never delete anything, saw: {seen:?}"
    );
}

/// DMN-105: a real prune removes an unprotected image one at a time (a
/// per-id DELETE) and never calls the Engine's bulk `/images/prune`.
#[tokio::test]
async fn prune_images_real_run_removes_one_at_a_time() {
    use asc_daemon::daemon::api::{ApiState, PruneTarget};

    let (docker_cfg, dir, hits) = test_cfg();
    let mut config = asc_daemon::daemon::config::Config::default();
    config.daemon.apps_dir = dir.path().join("apps");
    config.docker = docker_cfg;
    install_docker_app(&config, "web", "ghcr.io/acme/hello:1.0");

    let state = ApiState::new(config, "test-token".into());
    let result = state
        .prune_docker(root_ctx(), PruneTarget::Images, false, false)
        .await
        .unwrap();

    assert!(
        !result
            .removed
            .contains(&"ghcr.io/acme/hello:1.0".to_string()),
        "the protected image must survive a real run too"
    );
    assert_eq!(
        result.reclaimed_bytes, 52428800,
        "only the dangling image's bytes"
    );

    let seen = hits.lock().unwrap().clone();
    assert!(
        seen.iter()
            .any(|h| h.starts_with("DELETE") && h.contains("bbbb2222")),
        "the unprotected image must be removed by its own id, saw: {seen:?}"
    );
    assert!(
        !seen.iter().any(|h| h.contains("/images/prune")),
        "prune must never call the Engine's bulk endpoint, saw: {seen:?}"
    );
}

/// DMN-105: a named volume an installed app declares must never be removed.
#[tokio::test]
async fn prune_volumes_protects_a_declared_named_volume() {
    use asc_daemon::daemon::api::{ApiState, PruneTarget};

    let (docker_cfg, dir, hits) = test_cfg();
    let mut config = asc_daemon::daemon::config::Config::default();
    config.daemon.apps_dir = dir.path().join("apps");
    config.docker = docker_cfg;

    // A docker app whose settings declare the fixture's "shared-data" named
    // volume — the second fixture volume, "orphan", is declared by nothing.
    use asc_daemon::daemon::apps::AppStore;
    use asc_daemon::daemon::apps::meta::{AppMeta, DesiredState, Owner, Runtime};
    let store = AppStore::new(config.daemon.apps_dir.clone());
    store
        .save(&AppMeta {
            id: "web".into(),
            uuid: None,
            name: "web".into(),
            custom_name: None,
            owner: Owner {
                uid: 0,
                name: "root".into(),
            },
            version: Some("v1.0.0".into()),
            source: Some("test:local".into()),
            branch: None,
            repo_path: None,
            package: None,
            install_method: None,
            desired_state: DesiredState::Stopped,
            quota: None,
            runtime: Runtime::Docker {
                container: "asc-web".into(),
                image_source: None,
            },
        })
        .unwrap();
    let app_dir = store.app_dir("web").unwrap();
    std::fs::create_dir_all(app_dir.join("repository")).unwrap();
    std::fs::create_dir_all(app_dir.join("config")).unwrap();
    std::fs::write(
        app_dir.join("repository/asc.yaml"),
        "name: web\nversion: '1'\ntype: docker\nsettings: ./asc.settings.yaml\n\
         runtime:\n  image: ghcr.io/acme/hello:1.0\n",
    )
    .unwrap();
    std::fs::write(
        app_dir.join("repository/asc.settings.yaml"),
        "settings:\n  - { key: shared, type: volumes, \
         default: [\"shared-data:/data\"] }\n",
    )
    .unwrap();

    let state = ApiState::new(config, "test-token".into());
    let volumes = state.list_volumes(root_ctx()).await.unwrap();
    let shared = volumes.iter().find(|v| v.name == "shared-data").unwrap();
    assert!(shared.asc_protected);
    let orphan = volumes.iter().find(|v| v.name == "orphan").unwrap();
    assert!(!orphan.asc_protected);

    let result = state
        .prune_docker(root_ctx(), PruneTarget::Volumes, false, false)
        .await
        .unwrap();
    assert!(!result.removed.contains(&"shared-data".to_string()));
    assert!(result.removed.contains(&"orphan".to_string()));

    let seen = hits.lock().unwrap().clone();
    assert!(
        seen.iter()
            .any(|h| h.starts_with("DELETE") && h.contains("orphan")),
        "the unprotected volume must be removed by name, saw: {seen:?}"
    );
    assert!(
        !seen
            .iter()
            .any(|h| h.starts_with("DELETE") && h.contains("shared-data")),
        "the protected volume must never be deleted, saw: {seen:?}"
    );
}
