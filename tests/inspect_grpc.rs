//! DMN-098: `InspectPackage` over gRPC — what the platform's install dialog
//! calls before installing anything, so it can tell the operator that a
//! package is a stack and which apps it is about to put on the node. Runs as
//! its own test binary; no registry sources are involved.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use asc_daemon::daemon::api::proto::v1 as pb;
use asc_daemon::daemon::api::{self, ApiState};
use asc_daemon::daemon::config::Config;

const TOKEN: &str = "test-token-1234";

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git must be installed to run this test");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

async fn spawn_server(state: Arc<ApiState>) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, api::router(state)).await.unwrap();
    });
    addr
}

fn with_auth<T>(mut request: tonic::Request<T>) -> tonic::Request<T> {
    let value: tonic::metadata::MetadataValue<_> = format!("Bearer {TOKEN}").parse().unwrap();
    request.metadata_mut().insert("authorization", value);
    request
}

async fn channel(addr: std::net::SocketAddr) -> tonic::transport::Channel {
    tonic::transport::Channel::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap()
}

#[tokio::test]
async fn grpc_inspect_package_reads_a_stack_and_an_app() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: git is not available");
        return;
    }
    let ws = tempfile::tempdir().unwrap();

    // A monorepo with a stack in gameservers/demo and a plain app in web/solo
    // — the two shapes the dialog must tell apart.
    let repo = ws.path().join("monorepo");
    let stack_dir = repo.join("gameservers/demo");
    fs::create_dir_all(stack_dir.join("server")).unwrap();
    fs::write(
        stack_dir.join("server/asc.yaml"),
        "name: demo-server\nversion: 1.6.0\ntype: native\ntitle: Demo server\nrequirements:\n  ram: 4G\n  disk: 80G\n  cpu: 2\nruntime:\n  start: ./run.sh\n",
    )
    .unwrap();
    fs::create_dir_all(stack_dir.join("extras")).unwrap();
    fs::write(
        stack_dir.join("extras/asc.yaml"),
        "name: demo-extras\nversion: 1.6.0\ntype: native\nruntime:\n  start: ./run.sh\n",
    )
    .unwrap();
    fs::write(
        stack_dir.join("asc.stack.yaml"),
        "name: demo\nversion: 1.6.0\ntitle: Demo stack\napps:\n  - { name: server, path: ./server }\n  - { name: extras, path: ./extras, optional: true, depends_on: [server] }\n",
    )
    .unwrap();
    fs::create_dir_all(repo.join("web/solo")).unwrap();
    fs::write(
        repo.join("web/solo/asc.yaml"),
        "name: solo\nversion: 2.0.0\ntype: native\nruntime:\n  start: ./run.sh\n",
    )
    .unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let url = repo.display().to_string().replace('\\', "/");

    let mut config = Config::default();
    config.daemon.data_dir = ws.path().join("data");
    config.daemon.apps_dir = ws.path().join("apps");
    let state = ApiState::new(config, TOKEN.into());
    let addr = spawn_server(state).await;
    let mut apps = pb::app_service_client::AppServiceClient::new(channel(addr).await);

    let stack = apps
        .inspect_package(with_auth(tonic::Request::new(pb::InspectPackageRequest {
            git_url: url.clone(),
            branch: None,
            tag: None,
            path: Some("gameservers/demo".into()),
        })))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(stack.kind, pb::PackageKind::Stack as i32);
    assert_eq!(stack.name, "demo");
    assert_eq!(stack.title.as_deref(), Some("Demo stack"));
    assert_eq!(stack.apps.len(), 2);

    let server = &stack.apps[0];
    assert_eq!(server.name, "server");
    // The id the app installs under is its own manifest name, not the
    // stack-local one — that is what the dialog lists.
    assert_eq!(server.app_id, "demo-server");
    assert_eq!(server.title.as_deref(), Some("Demo server"));
    assert!(!server.optional);
    let requirements = server.requirements.as_ref().expect("requirements");
    assert_eq!(requirements.ram.as_deref(), Some("4G"));
    assert_eq!(requirements.cpu, Some(2.0));

    let extras = &stack.apps[1];
    assert!(extras.optional, "optional apps are reported as such");
    assert_eq!(extras.depends_on, ["server"]);

    // The same call on a plain app: no apps list, its own requirements.
    let app = apps
        .inspect_package(with_auth(tonic::Request::new(pb::InspectPackageRequest {
            git_url: url,
            branch: None,
            tag: None,
            path: Some("web/solo".into()),
        })))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(app.kind, pb::PackageKind::App as i32);
    assert_eq!(app.name, "solo");
    assert_eq!(app.version, "2.0.0");
    assert!(app.apps.is_empty());

    // Nothing was installed and no app directory was created by any of this.
    assert!(!ws.path().join("apps/demo-server").exists());
    assert!(!ws.path().join("apps/solo").exists());
}

/// DMN-106: a repository with no `asc.yaml`/`asc.stack.yaml` used to fail the
/// whole inspect (`Manifest::load` errors out); it must now come back as
/// `PACKAGE_KIND_UNSPECIFIED` with whatever install methods were detected,
/// so the install dialog can offer "found, not supported" instead of
/// treating the repository as unrecognized.
#[tokio::test]
async fn grpc_inspect_package_without_a_manifest_reports_unknown_and_detected_methods() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: git is not available");
        return;
    }
    let ws = tempfile::tempdir().unwrap();
    let repo = ws.path().join("dockerfile-only");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("Dockerfile"), "FROM alpine\nEXPOSE 8080\n").unwrap();
    fs::write(
        repo.join("docker-compose.yml"),
        "services:\n  web:\n    build: .\n",
    )
    .unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let url = repo.display().to_string().replace('\\', "/");

    let mut config = Config::default();
    config.daemon.data_dir = ws.path().join("data");
    config.daemon.apps_dir = ws.path().join("apps");
    let state = ApiState::new(config, TOKEN.into());
    let addr = spawn_server(state).await;
    let mut apps = pb::app_service_client::AppServiceClient::new(channel(addr).await);

    let response = apps
        .inspect_package(with_auth(tonic::Request::new(pb::InspectPackageRequest {
            git_url: url,
            branch: None,
            tag: None,
            path: None,
        })))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(response.kind, pb::PackageKind::Unspecified as i32);
    assert!(response.auth_required.is_none());
    assert!(response.apps.is_empty());

    let kinds: Vec<i32> = response.methods.iter().map(|m| m.kind).collect();
    assert!(kinds.contains(&(pb::InstallMethodKind::Dockerfile as i32)));
    assert!(kinds.contains(&(pb::InstallMethodKind::DockerCompose as i32)));
    // Dockerfile is installable since DMN-107 — unlike the docker-compose
    // project sitting right next to it in the same repository, which is not.
    let dockerfile = response
        .methods
        .iter()
        .find(|m| m.kind == pb::InstallMethodKind::Dockerfile as i32)
        .unwrap();
    assert!(dockerfile.supported);
    assert!(dockerfile.unsupported_reason.is_empty());
    assert_eq!(dockerfile.files, vec!["Dockerfile"]);
    // Whether docker_compose itself is supported depends on whether this
    // test host actually has the `docker compose` plugin (DMN-109) — outside
    // this test's control, so it only checks that `supported` and
    // `unsupported_reason` agree with each other, not which way.
    let compose = response
        .methods
        .iter()
        .find(|m| m.kind == pb::InstallMethodKind::DockerCompose as i32)
        .unwrap();
    assert_eq!(compose.supported, compose.unsupported_reason.is_empty());
}
