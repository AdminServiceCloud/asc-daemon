//! Docker Engine API client over the unix socket (configurable path).
//!
//! The daemon manages containers through the Engine API — **not** the
//! `docker` CLI — so rootless setups or a non-standard socket only need the
//! `[docker] socket` config. Control-plane operations are synchronous (the
//! app driver runs them via [`block_on`]); the console uses the async
//! streaming helpers directly on the API runtime.

use std::collections::HashMap;
use std::future::Future;

use anyhow::{Result, anyhow};
use bollard::Docker;
use bollard::auth::DockerCredentials;
use bollard::container::AttachContainerResults;
use bollard::errors::Error as BollardError;
use bollard::moby::buildkit::v1::{StatusResponse, Vertex};
use bollard::models::{
    BuildInfoAux, ContainerCreateBody, HostConfig, PortBinding, ResourcesUlimits, RestartPolicy,
    RestartPolicyNameEnum,
};
use bollard::query_parameters::{
    AttachContainerOptions, BuildImageOptionsBuilder, BuilderVersion, CreateContainerOptions,
    CreateImageOptions, LogsOptions, RemoveContainerOptions, StartContainerOptions, StatsOptions,
    StopContainerOptions,
};
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, trace, warn};

use crate::daemon::config::DockerConfig;
use crate::daemon::i18n::{Msg, t, tf};
use crate::daemon::progress;

/// Seconds the Engine waits on stop before killing the container.
const STOP_TIMEOUT_SECS: i64 = 10;

/// Open-file soft/hard limit given to every container, replacing the
/// Engine's own default (1024). A handful of game servers — 7 Days to Die's
/// EOS SDK is the documented case — hang or crash during startup on that
/// default with no clearer symptom than silence past their first log line;
/// 10240 is the value the affected games' own maintainers recommend, and a
/// higher fd limit is essentially free for everything else.
const CONTAINER_NOFILE_LIMIT: i64 = 10240;

/// Transport(s) a published port is forwarded on.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PortProtocol {
    #[default]
    Tcp,
    Udp,
    /// Both TCP and UDP, forwarded on the same port.
    Both,
}

impl PortProtocol {
    /// Docker transport keywords (`"tcp"`, `"udp"`) this protocol publishes.
    pub fn transports(self) -> &'static [&'static str] {
        match self {
            PortProtocol::Tcp => &["tcp"],
            PortProtocol::Udp => &["udp"],
            PortProtocol::Both => &["tcp", "udp"],
        }
    }
}

/// One published port: the `host` port the user picked and the `container`
/// port the package author fixed (the `container:` field of a `type: ports`
/// setting, DMN-052). A package that declares no container port publishes
/// host == container, which is what every package did before the field
/// existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishedPort {
    pub host: u16,
    pub container: u16,
    #[serde(default)]
    pub protocol: PortProtocol,
}

impl PublishedPort {
    /// A port published straight through, host == container.
    pub fn direct(port: u16, protocol: PortProtocol) -> Self {
        Self {
            host: port,
            container: port,
            protocol,
        }
    }

    /// Whether the host and container sides differ — the case worth showing
    /// to the user as a mapping instead of a bare port number.
    pub fn is_remapped(self) -> bool {
        self.host != self.container
    }

    /// The daemon's normalized binding keys, one per transport
    /// (`"8080:3000/tcp"`). Both sides of the drift check ([`AppliedConfig`]
    /// and `pkg::refresh`) build the same strings, so a changed host port —
    /// invisible in the Engine's own `ExposedPorts` keys, which name only the
    /// container side — still reads as drift and triggers a recreate.
    pub fn binding_keys(self) -> impl Iterator<Item = String> {
        let (host, container) = (self.host, self.container);
        self.protocol
            .transports()
            .iter()
            .map(move |transport| binding_key(host, container, transport))
    }
}

/// `"<host>:<container>/<transport>"` — see [`PublishedPort::binding_keys`].
fn binding_key(host: u16, container: u16, transport: &str) -> String {
    format!("{host}:{container}/{transport}")
}
/// Client connect/request timeout, seconds.
const CONNECT_TIMEOUT_SECS: u64 = 120;

/// Connect to the Docker Engine over the configured unix socket.
///
/// Connection is lazy (bollard connects on first request), so this only
/// fails fast when the socket file is missing; live errors surface per call.
pub fn connect(cfg: &DockerConfig) -> Result<Docker> {
    let socket = cfg.socket.to_string_lossy();
    Docker::connect_with_unix(&socket, CONNECT_TIMEOUT_SECS, bollard::API_DEFAULT_VERSION)
        .map_err(|err| friendly(cfg, err))
}

/// Map a Docker error to a user-facing one. An Engine response (any HTTP
/// status) proves Docker is reachable — pass its own message through instead
/// of blaming the socket. The same goes for an error the Engine reported
/// *inside* a streamed response body (a failing build or pull): the socket
/// was fine, the work wasn't. A host without the docker binary has Docker
/// missing, not stopped — say that and how to install it instead of asking
/// whether the daemon is running.
fn friendly(cfg: &DockerConfig, err: BollardError) -> anyhow::Error {
    if status_of(&err).is_some() || matches!(err, BollardError::DockerStreamError { .. }) {
        return anyhow!("{err}");
    }
    if !docker_binary_present() {
        return anyhow!("{}: {err}", t(Msg::ErrDockerNotFound));
    }
    anyhow!(
        "{}: {err}",
        tf(Msg::ErrDockerUnreachable, cfg.socket.display())
    )
}

/// Whether a `docker` executable is anywhere on PATH.
fn docker_binary_present() -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join("docker").is_file()))
        .unwrap_or(false)
}

/// HTTP status carried by a Docker Engine error response, if any.
fn status_of(err: &BollardError) -> Option<u16> {
    match err {
        BollardError::DockerResponseServerError { status_code, .. } => Some(*status_code),
        _ => None,
    }
}

/// Run a future to completion on a fresh current-thread runtime.
///
/// Driver operations are infrequent control-plane calls and never run inside
/// an ambient async context (the CLI is synchronous; the API wraps driver
/// calls in `spawn_blocking`), so a throwaway runtime per call is safe.
pub fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("cannot build docker runtime")
        .block_on(future)
}

// ── Synchronous control-plane operations (app driver) ───────────────────────

/// Start a container. A 304 (already started) is treated as success.
pub fn start(cfg: &DockerConfig, container: &str) -> Result<()> {
    block_on(async {
        let docker = connect(cfg)?;
        match docker
            .start_container(container, None::<StartContainerOptions>)
            .await
        {
            Ok(()) => Ok(()),
            Err(e) if status_of(&e) == Some(304) => Ok(()),
            Err(e) => Err(friendly(cfg, e)),
        }
    })
}

/// Stop a container (graceful, then kill after the timeout). 304 = already stopped.
pub fn stop(cfg: &DockerConfig, container: &str) -> Result<()> {
    block_on(async {
        let docker = connect(cfg)?;
        let opts = StopContainerOptions {
            t: Some(STOP_TIMEOUT_SECS as i32),
            ..Default::default()
        };
        match docker.stop_container(container, Some(opts)).await {
            Ok(()) => Ok(()),
            Err(e) if status_of(&e) == Some(304) => Ok(()),
            Err(e) => Err(friendly(cfg, e)),
        }
    })
}

pub fn restart(cfg: &DockerConfig, container: &str) -> Result<()> {
    block_on(async {
        let docker = connect(cfg)?;
        docker
            .restart_container(container, None)
            .await
            .map_err(|e| friendly(cfg, e))
    })
}

/// Whether the container exists and is running. A missing container (404)
/// reads as not running.
pub fn running(cfg: &DockerConfig, container: &str) -> Result<bool> {
    block_on(async {
        let docker = connect(cfg)?;
        match docker.inspect_container(container, None).await {
            Ok(info) => Ok(info.state.and_then(|s| s.running).unwrap_or(false)),
            Err(e) if status_of(&e) == Some(404) => Ok(false),
            Err(e) => Err(friendly(cfg, e)),
        }
    })
}

/// The parts of a container's configuration the daemon manages, read back
/// from inspect for settings-drift detection (see `pkg::refresh`).
#[derive(Debug)]
pub struct AppliedConfig {
    /// `Config.Env` — includes the image's own variables.
    pub env: Vec<String>,
    /// `HostConfig.Binds`, sorted.
    pub binds: Vec<String>,
    /// Published ports as normalized binding keys (`"8080:3000/tcp"`),
    /// sorted — see [`PublishedPort::binding_keys`].
    pub ports: Vec<String>,
    /// `HostConfig.NanoCpus`; 0 = unlimited.
    pub nano_cpus: i64,
    /// `HostConfig.Memory`, bytes; 0 = unlimited.
    pub memory: i64,
    /// `Config.Cmd` — a `start_command` override lands here.
    pub cmd: Option<Vec<String>>,
}

/// Inspect the daemon-managed configuration of a container. `None` when the
/// container does not exist (404).
pub fn container_applied(cfg: &DockerConfig, container: &str) -> Result<Option<AppliedConfig>> {
    block_on(async {
        let docker = connect(cfg)?;
        match docker.inspect_container(container, None).await {
            Ok(info) => {
                let config = info.config.unwrap_or_default();
                let host = info.host_config.unwrap_or_default();
                let mut ports: Vec<String> = host
                    .port_bindings
                    .map(|map| {
                        map.into_iter()
                            .map(|(key, bindings)| applied_binding_key(&key, bindings.as_deref()))
                            .collect()
                    })
                    .unwrap_or_default();
                ports.sort();
                let mut binds = host.binds.unwrap_or_default();
                binds.sort();
                Ok(Some(AppliedConfig {
                    env: config.env.unwrap_or_default(),
                    binds,
                    ports,
                    nano_cpus: host.nano_cpus.unwrap_or(0),
                    memory: host.memory.unwrap_or(0),
                    cmd: config.cmd,
                }))
            }
            Err(e) if status_of(&e) == Some(404) => Ok(None),
            Err(e) => Err(friendly(cfg, e)),
        }
    })
}

/// Normalize one `HostConfig.PortBindings` entry — Engine key
/// (`"3000/tcp"`, the **container** side) plus its host bindings — into the
/// daemon's `"<host>:<container>/<transport>"` form.
///
/// A binding without an explicit host port (the Engine picks an ephemeral
/// one) is not something the daemon ever creates: it reads back as `auto`,
/// which matches no desired key and so recreates the container onto the
/// ports the settings actually ask for.
fn applied_binding_key(key: &str, bindings: Option<&[PortBinding]>) -> String {
    let (container, transport) = key.split_once('/').unwrap_or((key, "tcp"));
    let host = bindings
        .and_then(|list| list.first())
        .and_then(|binding| binding.host_port.as_deref())
        .filter(|host| !host.is_empty())
        .unwrap_or("auto");
    format!("{host}:{container}/{transport}")
}

/// Force-remove the container. A missing container (404) is success.
pub fn remove(cfg: &DockerConfig, container: &str) -> Result<()> {
    block_on(async {
        let docker = connect(cfg)?;
        let opts = RemoveContainerOptions {
            force: true,
            ..Default::default()
        };
        match docker.remove_container(container, Some(opts)).await {
            Ok(()) => Ok(()),
            Err(e) if status_of(&e) == Some(404) => Ok(()),
            Err(e) => Err(friendly(cfg, e)),
        }
    })
}

/// One-shot resource counters of a container, straight off the Engine's
/// stats endpoint.
pub struct ContainerUsage {
    /// Cumulative CPU time, microseconds.
    pub cpu_time_micros: u64,
    /// Resident memory, bytes.
    pub memory_bytes: u64,
    /// Bytes read from/written to block devices since the container started.
    /// `None` on a cgroup v1 host, where the Engine omits this field.
    pub disk_read_bytes: Option<u64>,
    pub disk_write_bytes: Option<u64>,
    /// Bytes received/sent on the container's network namespace since it
    /// started, summed across all its interfaces. `None` when the container
    /// uses `network_mode: none` (no interfaces to report).
    pub net_rx_bytes: Option<u64>,
    pub net_tx_bytes: Option<u64>,
}

/// Sum of `io_service_bytes_recursive` entries by op (read/write). Only
/// `io_service_bytes_recursive` survives on a cgroup v2 host — every other
/// `ContainerBlkioStats` field is cgroup v1-only and always `None` there.
fn sum_blkio_bytes(
    blkio: Option<bollard::models::ContainerBlkioStats>,
) -> (Option<u64>, Option<u64>) {
    let Some(entries) = blkio.and_then(|b| b.io_service_bytes_recursive) else {
        return (None, None);
    };
    let (mut read, mut write) = (0u64, 0u64);
    for entry in &entries {
        let value = entry.value.unwrap_or(0);
        match entry.op.as_deref().map(str::to_ascii_lowercase).as_deref() {
            Some("read") => read += value,
            Some("write") => write += value,
            _ => {}
        }
    }
    (Some(read), Some(write))
}

/// Sum of `rx_bytes`/`tx_bytes` across every network interface of the
/// container's namespace.
fn sum_network_bytes(
    networks: Option<std::collections::HashMap<String, bollard::models::ContainerNetworkStats>>,
) -> (Option<u64>, Option<u64>) {
    let Some(networks) = networks else {
        return (None, None);
    };
    let (mut rx, mut tx) = (0u64, 0u64);
    for stats in networks.values() {
        rx += stats.rx_bytes.unwrap_or(0);
        tx += stats.tx_bytes.unwrap_or(0);
    }
    (Some(rx), Some(tx))
}

/// One-shot resource counters of a container. `None` when the container is
/// missing (404) or the Engine reports no memory usage (not running).
pub fn stats_usage(cfg: &DockerConfig, container: &str) -> Result<Option<ContainerUsage>> {
    block_on(async {
        let docker = connect(cfg)?;
        let opts = StatsOptions {
            stream: false,
            one_shot: true,
        };
        let mut stream = docker.stats(container, Some(opts));
        match stream.next().await {
            Some(Ok(stats)) => {
                let Some(memory_bytes) = stats.memory_stats.and_then(|m| m.usage) else {
                    return Ok(None);
                };
                // Engine reports CPU time in nanoseconds.
                let Some(cpu_time_micros) = stats
                    .cpu_stats
                    .and_then(|c| c.cpu_usage)
                    .and_then(|u| u.total_usage)
                    .map(|n| n / 1_000)
                else {
                    return Ok(None);
                };
                let (disk_read_bytes, disk_write_bytes) = sum_blkio_bytes(stats.blkio_stats);
                let (net_rx_bytes, net_tx_bytes) = sum_network_bytes(stats.networks);
                Ok(Some(ContainerUsage {
                    cpu_time_micros,
                    memory_bytes,
                    disk_read_bytes,
                    disk_write_bytes,
                    net_rx_bytes,
                    net_tx_bytes,
                }))
            }
            Some(Err(e)) if status_of(&e) == Some(404) => Ok(None),
            Some(Err(e)) => Err(friendly(cfg, e)),
            None => Ok(None),
        }
    })
}

/// Size of an image on the host, in bytes. `None` when the image has not
/// been pulled yet (404).
pub fn image_size(cfg: &DockerConfig, image: &str) -> Result<Option<u64>> {
    block_on(async {
        let docker = connect(cfg)?;
        match docker.inspect_image(image).await {
            Ok(info) => Ok(info.size.map(|s| s.max(0) as u64)),
            Err(e) if status_of(&e) == Some(404) => Ok(None),
            Err(e) => Err(friendly(cfg, e)),
        }
    })
}

/// Pull `image` if it is not already present locally; a no-op otherwise.
/// Lets a caller inspect the image (e.g. [`image_uid_gid`]) before it is
/// known to exist on the host, without duplicating [`create`]'s own
/// pull-on-404 handling.
pub fn ensure_pulled(cfg: &DockerConfig, image: &str, auth: Option<&RegistryAuth>) -> Result<()> {
    block_on(async {
        let docker = connect(cfg)?;
        match docker.inspect_image(image).await {
            Ok(_) => Ok(()),
            Err(e) if status_of(&e) == Some(404) => pull(&docker, image, auth)
                .await
                .map_err(|e| anyhow!("{}: {e}", tf(Msg::ErrImagePull, image))),
            Err(e) => Err(friendly(cfg, e)),
        }
    })
}

/// The numeric `(uid, gid)` the image's default `USER` runs as — `None` for
/// a named user (`steam`, `www-data`: resolving that needs the image's own
/// `/etc/passwd`, not available without running it), an unset user (root),
/// or a bare uid with no explicit group. Bind-mounted volumes are chowned to
/// this when known, so an image that `chown`s its own data directory on
/// first start does not hit EPERM against a root-owned bind mount — a
/// non-root process may only chown a path it already owns (DMN-038).
pub fn image_uid_gid(cfg: &DockerConfig, image: &str) -> Result<Option<(u32, u32)>> {
    block_on(async {
        let docker = connect(cfg)?;
        let info = docker
            .inspect_image(image)
            .await
            .map_err(|e| friendly(cfg, e))?;
        let user = info.config.and_then(|c| c.user).unwrap_or_default();
        let Some((uid, gid)) = user.split_once(':') else {
            return Ok(None);
        };
        Ok(uid.parse().ok().zip(gid.parse().ok()))
    })
}

/// Host mountpoint of a Docker named volume. `None` when the volume does
/// not exist yet (404) — the Engine creates it on first container use.
pub fn volume_mountpoint(cfg: &DockerConfig, name: &str) -> Result<Option<std::path::PathBuf>> {
    block_on(async {
        let docker = connect(cfg)?;
        match docker.inspect_volume(name).await {
            Ok(info) => Ok(Some(std::path::PathBuf::from(info.mountpoint))),
            Err(e) if status_of(&e) == Some(404) => Ok(None),
            Err(e) => Err(friendly(cfg, e)),
        }
    })
}

/// Last `tail` lines of the container's logs (non-follow), stdout+stderr.
pub fn logs_tail(cfg: &DockerConfig, container: &str, tail: usize) -> Result<String> {
    block_on(async {
        let docker = connect(cfg)?;
        let opts = LogsOptions {
            stdout: true,
            stderr: true,
            follow: false,
            tail: tail.to_string(),
            ..Default::default()
        };
        let mut stream = docker.logs(container, Some(opts));
        let mut out = String::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(log) => out.push_str(&String::from_utf8_lossy(&log.into_bytes())),
                // Container not created yet / removed: no logs, not an error.
                Err(e) if status_of(&e) == Some(404) => return Ok(String::new()),
                Err(e) => return Err(friendly(cfg, e)),
            }
        }
        Ok(out)
    })
}

/// Container definition for [`create`].
pub struct CreateSpec<'a> {
    pub name: &'a str,
    pub image: &'a str,
    /// Environment entries as `KEY=value`.
    pub env: Vec<String>,
    /// Ports to publish, each with its host and container side.
    pub ports: Vec<PublishedPort>,
    /// Volume binds as `host_path:container_path`.
    pub binds: Vec<String>,
    /// CPU quota in units of 1e-9 cores (Engine `NanoCpus`); `None` = unlimited.
    pub nano_cpus: Option<i64>,
    /// Memory limit in bytes (Engine `Memory`); `None` = unlimited.
    pub memory_bytes: Option<i64>,
    /// Start command override (`start_command` from asc.settings.yaml):
    /// replaces the image entrypoint, runs through `/bin/sh -c`.
    pub command: Option<String>,
    /// Keep the container's stdin open (Engine `OpenStdin`, like `docker run
    /// -i`) so attach input reaches the app.
    pub open_stdin: bool,
    /// Allocate a pseudo-TTY (Engine `Tty`, like `docker run -t`).
    pub tty: bool,
    /// Credentials for the image's registry (DMN-046); `None` = anonymous.
    pub registry_auth: Option<RegistryAuth>,
}

/// Credentials for one image registry, resolved from the `asc auth` store.
///
/// They travel to the Engine as the `X-Registry-Auth` header and the *Engine*
/// contacts the registry — the daemon itself never speaks to it, which is why
/// no TLS stack is needed on this side.
#[derive(Debug, Clone)]
pub struct RegistryAuth {
    pub username: String,
    pub token: String,
}

impl RegistryAuth {
    fn to_credentials(&self) -> DockerCredentials {
        DockerCredentials {
            username: Some(self.username.clone()),
            password: Some(self.token.clone()),
            ..Default::default()
        }
    }
}

/// Split an image reference into the `fromImage` and `tag` query parameters
/// of the Engine pull endpoint. A bare name gets an explicit `latest` — an
/// empty tag makes the Engine pull every tag of the repository. Digest
/// references go through whole: the Engine pulls by digest, no tag needed.
fn image_ref(image: &str) -> (&str, Option<&str>) {
    if image.contains('@') {
        return (image, None);
    }
    // A colon is the tag separator only after the last slash; earlier it is
    // a registry port (localhost:5000/app).
    let name_start = image.rfind('/').map_or(0, |i| i + 1);
    match image[name_start..].rfind(':') {
        Some(i) => (&image[..name_start + i], Some(&image[name_start + i + 1..])),
        None => (image, Some("latest")),
    }
}

/// Pull an image from its registry, waiting until the Engine finishes. Each
/// layer event is logged at debug level — the Engine gives no other way to
/// tell a slow pull from a stuck one — and, on a terminal, rendered as a
/// `docker pull`-style progress bar per layer, regardless of the log level.
async fn pull(
    docker: &Docker,
    image: &str,
    auth: Option<&RegistryAuth>,
) -> std::result::Result<(), BollardError> {
    let (from_image, tag) = image_ref(image);
    let opts = CreateImageOptions {
        from_image: Some(from_image.to_string()),
        tag: tag.map(str::to_string),
        ..Default::default()
    };
    let mut bars = progress::interactive().then(progress::LayerBars::new);
    let mut stream = docker.create_image(Some(opts), None, auth.map(RegistryAuth::to_credentials));
    while let Some(step) = stream.next().await {
        let step = step?;
        let bytes = step
            .progress_detail
            .as_ref()
            .and_then(|p| Some((p.current?, p.total?)));
        let status = step.status.as_deref().unwrap_or_default();
        let layer = step.id.as_deref().unwrap_or_default();
        debug!(
            image,
            layer,
            status,
            bytes = bytes
                .map(|(c, t)| format!("{c}/{t}"))
                .as_deref()
                .unwrap_or_default(),
            "pulling image"
        );
        if let Some(bars) = &mut bars {
            if layer.is_empty() {
                bars.header(status);
            } else {
                bars.update(layer, status, bytes);
            }
        }
    }
    if let Some(bars) = bars {
        bars.finish();
    }
    Ok(())
}

/// A local image build (DMN-050): the Engine builds `tag` from a Dockerfile
/// in the package repository, so a package can ship its own image instead of
/// (or beside) a prebuilt one on a registry.
pub struct BuildSpec<'a> {
    /// Build context directory (its contents are sent to the Engine as a tar).
    pub context_dir: &'a std::path::Path,
    /// Dockerfile path, relative to the context.
    pub dockerfile: &'a str,
    /// Tag for the built image.
    pub tag: &'a str,
    /// `--build-arg` values.
    pub args: &'a std::collections::BTreeMap<String, String>,
}

/// Build a Docker image from a Dockerfile shipped in the package (DMN-050).
/// The build context directory is streamed to the Engine as an in-memory tar;
/// the Engine builds `tag` and the daemon reuses it exactly like a pulled
/// image. The build always runs through the Engine's BuildKit backend
/// (`version=2`, over a bollard-managed session) rather than the legacy
/// builder — the legacy builder lacks Dockerfile syntax such as `COPY
/// --chmod`, which fails with "the --chmod option requires BuildKit"
/// otherwise; the session is what lets `BuildInfo.aux` decode BuildKit's own
/// progress frames instead of only the legacy builder's shape (without it,
/// the Engine's BuildKit-compat translation sends some progress lines as an
/// untyped protobuf blob there, and this crate aborts the whole build stream
/// trying to decode one). Progress comes from that trace, not from the
/// `stream` text lines the legacy builder used to emit: each step is logged
/// at debug level and, on a terminal, rendered as a `docker build`-style
/// progress bar per step, regardless of the log level. A build error
/// surfaces the Engine's own message.
pub fn build_image(cfg: &DockerConfig, spec: BuildSpec<'_>) -> Result<()> {
    let tar = tar_context(spec.context_dir)?;
    let session = build_session_id();
    // The build's own header: everything needed to tell an empty log apart
    // from a build that never started. `bars` says whether this process can
    // render progress at all — it cannot when the build runs inside the
    // daemon (stderr is the journal, not a terminal), which is the normal
    // case for `asc install` and the reason these lines are info, not debug.
    info!(
        tag = spec.tag,
        dockerfile = spec.dockerfile,
        context = %spec.context_dir.display(),
        context_bytes = tar.len(),
        build_args = spec.args.len(),
        session = %session,
        bars = progress::interactive(),
        "image build starting"
    );
    block_on(async {
        let docker = connect(cfg)?;
        let mut builder = BuildImageOptionsBuilder::new()
            .dockerfile(spec.dockerfile)
            .t(spec.tag)
            // Remove intermediate containers on success, like `docker build`.
            .rm(true)
            // Legacy builder doesn't understand `COPY --chmod`/`--chown`
            // extensions some package Dockerfiles rely on (DMN-050). The
            // session id just correlates this build with its side-channel
            // callback (auth), so any per-build id does.
            .version(BuilderVersion::BuilderBuildKit)
            .session(&session);
        if !spec.args.is_empty() {
            let args: HashMap<String, String> = spec
                .args
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            builder = builder.buildargs(&args);
        }
        let body = bollard::body_full(bytes::Bytes::from(tar));
        let mut stream = docker.build_image(builder.build(), None, Some(body));
        let mut bars = progress::interactive().then(progress::BuildBars::new);
        // Frame accounting: what the Engine actually sent, so a build with no
        // visible progress can be told apart from a build with no progress at
        // all. `traced` counting zero on a finished build means the BuildKit
        // side-channel degraded (session, Engine version, builder version) —
        // that is a defect, and it gets a warning of its own below.
        let mut frames = 0usize;
        let mut traced = 0usize;
        while let Some(step) = stream.next().await {
            let info = match step {
                Ok(info) => info,
                // The Engine reporting a failure inside the build stream:
                // a Dockerfile or BuildKit error, not a transport one, so it
                // gets the build's own context rather than `friendly`'s
                // connectivity wording.
                Err(BollardError::DockerStreamError { error }) => {
                    warn!(
                        tag = spec.tag,
                        frames, traced, "build stream reported an error"
                    );
                    return Err(anyhow!("{}: {error}", tf(Msg::ErrImageBuild, spec.tag)));
                }
                Err(e) => {
                    warn!(
                        tag = spec.tag,
                        frames,
                        traced,
                        error = %format!("{e:?}"),
                        "build stream aborted"
                    );
                    return Err(friendly(cfg, e));
                }
            };
            frames += 1;
            // The raw frame, for when the decoded view above is not enough
            // (`RUST_LOG=asc_daemon=trace`): one line per frame, verbatim.
            trace!(tag = spec.tag, frame = frames, "{info:?}");
            if let Some(detail) = &info.error_detail {
                let msg = detail.message.as_deref().unwrap_or("image build failed");
                warn!(
                    tag = spec.tag,
                    frames, traced, "build reported an error frame"
                );
                return Err(anyhow!("{}: {msg}", tf(Msg::ErrImageBuild, spec.tag)));
            }
            match &info.aux {
                Some(BuildInfoAux::BuildKit(trace)) => {
                    traced += 1;
                    build_trace(spec.tag, trace, bars.as_mut());
                }
                // The classic builder's final "here is your image" frame; with
                // BuildKit it is the only non-trace aux that ever shows up.
                Some(BuildInfoAux::Default(image)) => {
                    debug!(tag = spec.tag, image = ?image.id, "build produced an image id");
                }
                None => {}
            }
            // The legacy builder's text output. BuildKit sends none of it, but
            // it costs nothing to keep logging whatever does arrive.
            if let Some(line) = info
                .stream
                .as_deref()
                .map(str::trim_end)
                .filter(|l| !l.is_empty())
            {
                debug!(tag = spec.tag, "{line}");
            }
            // Layer-pull style frames (status/progressDetail) — the legacy
            // shape again, kept for the same reason.
            if let Some(status) = info.status.as_deref().filter(|s| !s.trim().is_empty()) {
                debug!(tag = spec.tag, id = ?info.id, "{status}");
            }
        }
        if let Some(bars) = bars {
            bars.finish();
        }
        if traced == 0 {
            // Not fatal: the image may well have been built. But it means the
            // build ran blind — no step ever reached the log or the bars —
            // and that is exactly the state this instrumentation exists to
            // name instead of leaving as a silent terminal.
            warn!(
                tag = spec.tag,
                frames,
                session = %session,
                "no BuildKit progress frames arrived: the build reported no steps \
                 (check the Engine's BuildKit support and the build session)"
            );
        } else {
            info!(tag = spec.tag, frames, traced, "image build finished");
        }
        Ok(())
    })
}

/// Render one frame of BuildKit's build trace: the vertices (Dockerfile
/// steps) that changed state, the byte progress reported inside them, and
/// the command output they produced. A step reaching a terminal state is
/// logged at info level, so the build is visible in `journalctl -u asc`
/// without turning debug logging on — that is the only progress a
/// non-terminal caller (the daemon serving `asc install`, a script) gets. The
/// noisier half (a step starting, byte counters, the step's own output) stays
/// at debug, and on a terminal everything is mirrored into the step bars.
fn build_trace(tag: &str, trace: &StatusResponse, mut bars: Option<&mut progress::BuildBars>) {
    for vertex in &trace.vertexes {
        // A vertex is announced before it runs; docker shows nothing for it
        // until it starts, and neither do we.
        let Some(state) = vertex_state(vertex) else {
            continue;
        };
        match &state {
            progress::StepState::Running => debug!(tag, step = vertex.name, "running"),
            terminal => info!(tag, step = vertex.name, "{}", terminal.label()),
        }
        if let Some(bars) = bars.as_mut() {
            bars.step(&vertex.digest, &vertex.name, state);
        }
    }
    for status in &trace.statuses {
        let bytes = (status.total > 0).then_some((status.current, status.total));
        // `name` is the action ("sha256:… extracting"), `id` the layer.
        let label = if status.name.is_empty() {
            status.id.as_str()
        } else {
            status.name.as_str()
        };
        debug!(tag, layer = status.id, "{label}");
        if let Some(bars) = bars.as_mut() {
            bars.step_status(&status.vertex, label, bytes);
        }
    }
    // A step's own output (compiler messages, package manager logs) — the
    // detail behind a failing build, so it goes to the log verbatim.
    for log in &trace.logs {
        for line in String::from_utf8_lossy(&log.msg).lines() {
            if !line.trim().is_empty() {
                debug!(tag, "{line}");
            }
        }
    }
}

/// What a vertex is doing, or `None` while it has not started yet. Cached
/// steps never "start" — BuildKit marks them done in the same frame.
fn vertex_state(vertex: &Vertex) -> Option<progress::StepState> {
    if !vertex.error.is_empty() {
        return Some(progress::StepState::Failed(vertex.error.clone()));
    }
    if vertex.cached {
        return Some(progress::StepState::Cached);
    }
    match (&vertex.started, &vertex.completed) {
        (Some(started), Some(completed)) => {
            let secs = (completed.seconds - started.seconds) as f64
                + f64::from(completed.nanos - started.nanos) / 1e9;
            Some(progress::StepState::Done(secs.max(0.0)))
        }
        (Some(_), None) => Some(progress::StepState::Running),
        _ => None,
    }
}

/// Id for a build's BuildKit session — opaque, unique per build, and with no
/// colon in it.
///
/// The colon matters: BuildKit registers a session under the id bollard sends
/// on the `/session` upgrade verbatim, but looks it up by everything *after*
/// the first colon (the prefix is its own namespacing convention for solver
/// vertices). An image tag as the id — `asc-local/app:latest` — therefore
/// leaves every build waiting on a session named `latest` that never
/// attaches, and BuildKit fails the first thing that needs the session (base
/// image metadata, which goes through the session's auth provider) with "no
/// active session for latest: context deadline exceeded".
fn build_session_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("asc-build-{}-{}", std::process::id(), now.as_nanos())
}

/// Pack a build context directory into an uncompressed tar in memory. The
/// Engine wants a tar stream, and the contexts we build (a package repository
/// checkout) are small, so buffering is fine.
fn tar_context(dir: &std::path::Path) -> Result<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());
    builder
        .append_dir_all("", dir)
        .map_err(|e| anyhow!("cannot pack build context {}: {e}", dir.display()))?;
    builder
        .into_inner()
        .map_err(|e| anyhow!("cannot finalize build context tar: {e}"))
}

/// Create (but do not start) a container from a spec. Used by the installer.
/// An image missing on the host is pulled from its registry automatically.
pub fn create(cfg: &DockerConfig, spec: CreateSpec<'_>) -> Result<()> {
    block_on(async {
        let docker = connect(cfg)?;

        // The Engine names a port by its **container** side; the host side
        // lives in the binding. They are equal unless the package fixed a
        // container port of its own (`container:`, DMN-052).
        let mut exposed_ports: Vec<String> = Vec::new();
        let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
        for port in &spec.ports {
            for transport in port.protocol.transports() {
                let key = format!("{}/{transport}", port.container);
                exposed_ports.push(key.clone());
                port_bindings.insert(
                    key,
                    Some(vec![PortBinding {
                        host_ip: None,
                        host_port: Some(port.host.to_string()),
                    }]),
                );
            }
        }

        let host_config = HostConfig {
            port_bindings: (!port_bindings.is_empty()).then_some(port_bindings),
            binds: (!spec.binds.is_empty()).then_some(spec.binds.clone()),
            restart_policy: Some(RestartPolicy {
                name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                maximum_retry_count: None,
            }),
            nano_cpus: spec.nano_cpus,
            memory: spec.memory_bytes,
            ulimits: Some(vec![ResourcesUlimits {
                name: Some("nofile".to_string()),
                soft: Some(CONTAINER_NOFILE_LIMIT),
                hard: Some(CONTAINER_NOFILE_LIMIT),
            }]),
            ..Default::default()
        };

        let config = ContainerCreateBody {
            image: Some(spec.image.to_string()),
            // A start_command replaces whatever the image would run: the
            // entrypoint becomes the shell so the command can use arguments
            // and env references.
            entrypoint: spec
                .command
                .as_ref()
                .map(|_| vec!["/bin/sh".to_string(), "-c".to_string()]),
            cmd: spec.command.as_ref().map(|c| vec![c.clone()]),
            env: (!spec.env.is_empty()).then(|| spec.env.clone()),
            open_stdin: spec.open_stdin.then_some(true),
            tty: spec.tty.then_some(true),
            exposed_ports: (!exposed_ports.is_empty()).then_some(exposed_ports),
            host_config: Some(host_config),
            ..Default::default()
        };

        let options = CreateContainerOptions {
            name: Some(spec.name.to_string()),
            ..Default::default()
        };
        match docker
            .create_container(Some(options.clone()), config.clone())
            .await
        {
            Ok(_) => {}
            // 404 = the image is not on the host: pull it and retry once.
            Err(e) if status_of(&e) == Some(404) => {
                info!(image = spec.image, "image not found locally, pulling");
                pull(&docker, spec.image, spec.registry_auth.as_ref())
                    .await
                    .map_err(|e| anyhow!("{}: {e}", tf(Msg::ErrImagePull, spec.image)))?;
                docker
                    .create_container(Some(options), config)
                    .await
                    .map_err(|e| friendly(cfg, e))?;
            }
            Err(e) => return Err(friendly(cfg, e)),
        }
        Ok(())
    })
}

// ── Async streaming operations (WebSocket console) ──────────────────────────

/// Follow-mode logs as a stream of UTF-8 text lines (trailing newline
/// stripped). Timestamps are included by the Engine.
pub async fn logs_follow(
    cfg: &DockerConfig,
    container: &str,
    tail: usize,
) -> Result<impl Stream<Item = Result<String>> + Send> {
    let docker = connect(cfg)?;
    let opts = LogsOptions {
        follow: true,
        stdout: true,
        stderr: true,
        timestamps: true,
        tail: tail.to_string(),
        ..Default::default()
    };
    // The stream owns its transport handle, so `docker` may drop here.
    let stream = docker.logs(container, Some(opts)).map(|item| {
        item.map(|log| {
            let mut line = String::from_utf8_lossy(&log.into_bytes()).into_owned();
            while line.ends_with('\n') || line.ends_with('\r') {
                line.pop();
            }
            line
        })
        .map_err(|e| anyhow!("docker logs: {e}"))
    });
    Ok(stream)
}

/// Interactive attach: bidirectional stdin/stdout to a running container.
pub async fn attach(cfg: &DockerConfig, container: &str) -> Result<AttachContainerResults> {
    let docker = connect(cfg)?;
    let opts = AttachContainerOptions {
        stdin: true,
        stdout: true,
        stderr: true,
        stream: true,
        logs: false,
        detach_keys: None,
    };
    docker
        .attach_container(container, Some(opts))
        .await
        .map_err(|e| friendly(cfg, e))
}

#[cfg(test)]
mod tests {
    use super::{BollardError, DockerConfig, Vertex, build_session_id, friendly, image_ref};
    use crate::daemon::progress::StepState;

    /// BuildKit announces a step before it runs and reports its outcome on
    /// the vertex itself — the whole of a build's visible progress.
    #[test]
    fn vertex_states_follow_buildkit_trace() {
        let pending = Vertex {
            name: "[2/7] RUN pnpm install".into(),
            ..Default::default()
        };
        assert!(
            super::vertex_state(&pending).is_none(),
            "an announced but unstarted step shows nothing"
        );

        let mut running = pending.clone();
        running.started = Some(Default::default());
        assert!(matches!(
            super::vertex_state(&running),
            Some(StepState::Running)
        ));

        let mut done = running.clone();
        done.completed = Some(Default::default());
        if let Some(ts) = &mut done.completed {
            ts.seconds = 3;
            ts.nanos = 500_000_000;
        }
        let Some(StepState::Done(secs)) = super::vertex_state(&done) else {
            panic!("a completed step reports its duration");
        };
        assert!((secs - 3.5).abs() < f64::EPSILON, "got {secs}");

        // A cached step never starts: BuildKit marks it in one frame.
        let cached = Vertex {
            cached: true,
            ..pending.clone()
        };
        assert!(matches!(
            super::vertex_state(&cached),
            Some(StepState::Cached)
        ));

        let failed = Vertex {
            error: "exit code 1".into(),
            ..pending
        };
        assert!(
            matches!(super::vertex_state(&failed), Some(StepState::Failed(e)) if e == "exit code 1")
        );
    }

    /// A colon in the session id makes BuildKit look the session up under
    /// whatever follows it — a lookup that never matches what bollard
    /// registered, so the build hangs until its deadline ("no active session
    /// for <suffix>").
    #[test]
    fn build_session_ids_are_unique_and_colon_free() {
        let (first, second) = (build_session_id(), build_session_id());
        assert!(!first.contains(':'), "colon in session id: {first}");
        assert_ne!(first, second, "session id must differ per build");
    }

    #[test]
    fn stream_errors_are_not_reported_as_unreachable() {
        let cfg = DockerConfig {
            socket: std::path::PathBuf::from("/var/run/docker.sock"),
        };
        let err = friendly(
            &cfg,
            BollardError::DockerStreamError {
                error: String::from("failed to resolve source metadata"),
            },
        );
        let msg = format!("{err:#}");
        assert!(msg.contains("failed to resolve source metadata"));
        assert!(
            !msg.contains("cannot reach Docker"),
            "the Engine answered — this is not a connectivity failure, got: {msg}"
        );
    }

    #[test]
    fn image_refs_split_into_name_and_tag() {
        assert_eq!(image_ref("nginx"), ("nginx", Some("latest")));
        assert_eq!(image_ref("nginx:1.27"), ("nginx", Some("1.27")));
        assert_eq!(
            image_ref("steamcmd/steamcmd:latest"),
            ("steamcmd/steamcmd", Some("latest"))
        );
        assert_eq!(
            image_ref("localhost:5000/app"),
            ("localhost:5000/app", Some("latest"))
        );
        assert_eq!(
            image_ref("localhost:5000/app:v2"),
            ("localhost:5000/app", Some("v2"))
        );
        assert_eq!(image_ref("redis@sha256:abc"), ("redis@sha256:abc", None));
    }
}
