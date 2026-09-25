//! Docker Engine API client over the unix socket (configurable path).
//!
//! The daemon manages containers through the Engine API — **not** the
//! `docker` CLI — so rootless setups or a non-standard socket only need the
//! `[docker] socket` config. Control-plane operations are synchronous (the
//! app driver runs them via [`block_on`]); the console uses the async
//! streaming helpers directly on the API runtime.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use anyhow::{Result, anyhow};
use bollard::Docker;
use bollard::auth::DockerCredentials;
use bollard::container::{AttachContainerResults, LogOutput};
use bollard::errors::Error as BollardError;
use bollard::exec::{CreateExecOptions, ResizeExecOptions, StartExecOptions, StartExecResults};
use bollard::moby::buildkit::v1::{StatusResponse, Vertex};
use bollard::models::{
    BuildInfoAux, ContainerCreateBody, ContainerSummary, HostConfig, PortBinding, ResourcesUlimits,
    RestartPolicy, RestartPolicyNameEnum,
};
use bollard::query_parameters::{
    AttachContainerOptions, BuildImageOptionsBuilder, BuilderVersion, CreateContainerOptions,
    CreateImageOptions, ListContainersOptions, ListImagesOptions, LogsOptions,
    RemoveContainerOptions, RemoveImageOptions, StartContainerOptions, StatsOptions,
    StopContainerOptions,
};
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWrite;
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

/// Freeze every process of a running container (`docker pause`).
pub fn pause(cfg: &DockerConfig, container: &str) -> Result<()> {
    block_on(async {
        let docker = connect(cfg)?;
        docker
            .pause_container(container)
            .await
            .map_err(|e| friendly(cfg, e))
    })
}

/// Resume a paused container (`docker unpause`).
pub fn unpause(cfg: &DockerConfig, container: &str) -> Result<()> {
    block_on(async {
        let docker = connect(cfg)?;
        docker
            .unpause_container(container)
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

/// Unix timestamp of the container's current run, for app uptime (DMN-089).
/// `None` when the container is missing, stopped, or has never started —
/// the Engine reports `"0001-01-01T00:00:00Z"` (Go's zero `time.Time`) for
/// the last case rather than omitting the field.
pub fn started_at(cfg: &DockerConfig, container: &str) -> Result<Option<i64>> {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    block_on(async {
        let docker = connect(cfg)?;
        match docker.inspect_container(container, None).await {
            Ok(info) => {
                let running = info.state.as_ref().and_then(|s| s.running).unwrap_or(false);
                if !running {
                    return Ok(None);
                }
                let Some(raw) = info.state.and_then(|s| s.started_at) else {
                    return Ok(None);
                };
                match OffsetDateTime::parse(&raw, &Rfc3339) {
                    Ok(parsed) if parsed.unix_timestamp() > 0 => Ok(Some(parsed.unix_timestamp())),
                    // Zero value or unparseable — not started, or an Engine
                    // version whose format this doesn't expect.
                    _ => Ok(None),
                }
            }
            Err(e) if status_of(&e) == Some(404) => Ok(None),
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
    /// `Image` — the id (`sha256:…`) of the image the container was
    /// created from. A re-pulled tag (DMN-120) moves the tag to a new id
    /// while the container keeps the old one: that difference is drift.
    pub image: Option<String>,
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
                    image: info.image,
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

// ── Host inventory (DMN-102) ────────────────────────────────────────────────

/// Compose writes the project and service names of every container it
/// creates into these labels. Lifting them out of the raw label map here
/// means no caller has to know how Compose spells its own keys.
const COMPOSE_PROJECT_LABEL: &str = "com.docker.compose.project";
const COMPOSE_SERVICE_LABEL: &str = "com.docker.compose.service";

/// One port row of a container as the Engine's container list reports it.
/// Unlike [`PublishedPort`], which describes what an app's *manifest asks
/// for*, this is what the Engine actually has: a port that is merely exposed
/// carries no `public` side at all.
pub struct ContainerPortInfo {
    pub private: u16,
    pub public: Option<u16>,
    /// `tcp`, `udp` or `sctp`; empty when the Engine did not say.
    pub protocol: String,
    /// Host bind address of a published port; empty for an exposed-only one.
    pub ip: String,
}

/// One container on the host — ASC-managed or not.
///
/// Deliberately not bollard's `ContainerSummary`: this module keeps its own
/// types at its boundary (see [`AppliedConfig`], [`ContainerUsage`],
/// [`PublishedPort`]), so the Engine's schema does not leak into the API
/// layer and a bollard upgrade cannot silently reshape the daemon's own
/// contract.
pub struct ContainerInfo {
    /// Full 64-hex id; abbreviating is the caller's business.
    pub id: String,
    /// Names with the Engine's leading `/` stripped.
    pub names: Vec<String>,
    pub image: String,
    pub image_id: String,
    /// `created`, `running`, `paused`, `restarting`, `removing`, `exited`,
    /// `dead`, `stopping` — passed through as the Engine spells it.
    pub state: String,
    /// Human-readable status line ("Up 3 hours").
    pub status: String,
    /// Creation time, unix seconds; 0 when the Engine omitted it.
    pub created: i64,
    pub ports: Vec<ContainerPortInfo>,
    pub labels: HashMap<String, String>,
    pub compose_project: Option<String>,
    pub compose_service: Option<String>,
    /// Only present when the caller asked for sizes.
    pub size_rw: Option<u64>,
    pub size_root_fs: Option<u64>,
    /// Networks the container is attached to.
    pub networks: Vec<String>,
}

/// Every container on the host, like `docker ps` (`all` = `docker ps -a`).
///
/// `with_size` maps to the Engine's `size=1`, which makes it walk each
/// container's writable layer — a visible pause on a busy node, so it stays
/// opt-in and off by default.
pub fn list_containers(
    cfg: &DockerConfig,
    all: bool,
    with_size: bool,
) -> Result<Vec<ContainerInfo>> {
    block_on(async {
        let docker = connect(cfg)?;
        let opts = ListContainersOptions {
            all,
            size: with_size,
            ..Default::default()
        };
        let summaries = docker
            .list_containers(Some(opts))
            .await
            .map_err(|e| friendly(cfg, e))?;
        Ok(summaries
            .into_iter()
            .map(|summary| container_info(summary, with_size))
            .collect())
    })
}

/// Map one Engine container summary onto [`ContainerInfo`].
fn container_info(summary: ContainerSummary, with_size: bool) -> ContainerInfo {
    let labels = summary.labels.unwrap_or_default();
    let networks = summary
        .network_settings
        .and_then(|settings| settings.networks)
        .map(|map| {
            let mut names: Vec<String> = map.into_keys().collect();
            names.sort();
            names
        })
        .unwrap_or_default();
    ContainerInfo {
        id: summary.id.unwrap_or_default(),
        names: summary
            .names
            .unwrap_or_default()
            .into_iter()
            // The Engine prefixes every name with '/' for historic reasons
            // ("links"); nobody downstream wants to see it.
            .map(|name| name.trim_start_matches('/').to_string())
            .collect(),
        image: summary.image.unwrap_or_default(),
        image_id: summary.image_id.unwrap_or_default(),
        state: summary.state.map(|s| s.to_string()).unwrap_or_default(),
        status: summary.status.unwrap_or_default(),
        created: summary.created.unwrap_or(0),
        ports: summary
            .ports
            .unwrap_or_default()
            .into_iter()
            .map(|port| ContainerPortInfo {
                private: port.private_port,
                public: port.public_port,
                protocol: port.typ.map(|t| t.to_string()).unwrap_or_default(),
                ip: port.ip.unwrap_or_default(),
            })
            .collect(),
        compose_project: labels.get(COMPOSE_PROJECT_LABEL).cloned(),
        compose_service: labels.get(COMPOSE_SERVICE_LABEL).cloned(),
        labels,
        // A negative size would mean the Engine sent something nonsensical;
        // treat it as "not reported" rather than wrapping around on the cast.
        size_rw: with_size
            .then_some(summary.size_rw)
            .flatten()
            .and_then(|v| u64::try_from(v).ok()),
        size_root_fs: with_size
            .then_some(summary.size_root_fs)
            .flatten()
            .and_then(|v| u64::try_from(v).ok()),
        networks,
    }
}

// ── Images, volumes, networks, disk usage (DMN-104) ─────────────────────────

/// One image on the host.
pub struct ImageInfo {
    /// Full 64-hex-with-`sha256:` id.
    pub id: String,
    /// `name:tag` references pointing at this image; empty for a "dangling"
    /// image (an old build the Engine could no longer name after a
    /// `docker build`/pull replaced its tag).
    pub tags: Vec<String>,
    /// Total size on disk, bytes.
    pub size: u64,
    /// Creation time, unix seconds.
    pub created: i64,
    pub labels: HashMap<String, String>,
    /// No tag references it — `docker images -f dangling=true`'s definition.
    pub dangling: bool,
}

/// Every image on the host (`docker images -a`'s full set, not just tagged
/// ones — a caller that wants only tagged images filters on `!dangling`).
pub fn list_images(cfg: &DockerConfig) -> Result<Vec<ImageInfo>> {
    block_on(async {
        let docker = connect(cfg)?;
        let opts = ListImagesOptions {
            all: true,
            ..Default::default()
        };
        let images = docker
            .list_images(Some(opts))
            .await
            .map_err(|e| friendly(cfg, e))?;
        Ok(images.into_iter().map(image_info).collect())
    })
}

fn image_info(summary: bollard::models::ImageSummary) -> ImageInfo {
    let tags: Vec<String> = summary
        .repo_tags
        .into_iter()
        .filter(|t| t != "<none>:<none>")
        .collect();
    ImageInfo {
        dangling: tags.is_empty(),
        id: summary.id,
        tags,
        size: summary.size.max(0) as u64,
        created: summary.created,
        labels: summary.labels,
    }
}

/// Remove one image by id or reference. A missing image (404) is success —
/// the caller was trying to get rid of it either way. Never forces past an
/// image still in use by a container: the daemon's own protected-set check
/// happens before this is ever called, and an Engine-side "in use" refusal
/// for anything else is a real reason to stop, not to override.
pub fn remove_image(cfg: &DockerConfig, id_or_ref: &str) -> Result<()> {
    block_on(async {
        let docker = connect(cfg)?;
        match docker
            .remove_image(id_or_ref, None::<RemoveImageOptions>, None)
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if status_of(&e) == Some(404) => Ok(()),
            Err(e) => Err(friendly(cfg, e)),
        }
    })
}

/// One Docker named volume on the host.
pub struct VolumeInfo {
    pub name: String,
    pub driver: String,
    pub mountpoint: String,
    /// Unix seconds; absent when the Engine did not report a parseable date.
    pub created_at: Option<i64>,
    pub labels: HashMap<String, String>,
    /// Containers currently referencing this volume; `None` when the Engine
    /// did not report usage data for it (the plain, non-verbose `/volumes`
    /// listing does not compute it for every driver).
    pub ref_count: Option<i64>,
    /// Disk space used, bytes; `None` for the same reason as `ref_count`, or
    /// when the driver does not support the figure (`-1` from the Engine).
    pub size_bytes: Option<u64>,
}

/// Every named volume on the host.
pub fn list_volumes(cfg: &DockerConfig) -> Result<Vec<VolumeInfo>> {
    block_on(async {
        let docker = connect(cfg)?;
        let response = docker
            .list_volumes(None::<bollard::query_parameters::ListVolumesOptions>)
            .await
            .map_err(|e| friendly(cfg, e))?;
        Ok(response
            .volumes
            .unwrap_or_default()
            .into_iter()
            .map(volume_info)
            .collect())
    })
}

fn volume_info(volume: bollard::models::Volume) -> VolumeInfo {
    let usage = volume.usage_data;
    VolumeInfo {
        name: volume.name,
        driver: volume.driver,
        mountpoint: volume.mountpoint,
        // The "time" bollard feature (enabled in Cargo.toml) makes
        // `BollardDate` a `time::OffsetDateTime` directly — no string parse.
        created_at: volume.created_at.map(|d| d.unix_timestamp()),
        labels: volume.labels,
        ref_count: usage.as_ref().map(|u| u.ref_count),
        size_bytes: usage.and_then(|u| u64::try_from(u.size).ok()),
    }
}

/// Remove one named volume. A missing volume (404) is success.
pub fn remove_volume(cfg: &DockerConfig, name: &str) -> Result<()> {
    block_on(async {
        let docker = connect(cfg)?;
        match docker
            .remove_volume(name, None::<bollard::query_parameters::RemoveVolumeOptions>)
            .await
        {
            Ok(()) => Ok(()),
            Err(e) if status_of(&e) == Some(404) => Ok(()),
            Err(e) => Err(friendly(cfg, e)),
        }
    })
}

/// One Docker network on the host. Read-only in this API — see
/// `docs/english/app-management.md` for why networks are never pruned here.
pub struct NetworkInfo {
    pub id: String,
    pub name: String,
    pub driver: String,
    /// `local` or `swarm`.
    pub scope: String,
    pub internal: bool,
    /// Unix seconds; absent when the Engine did not report a parseable date.
    pub created: Option<i64>,
    pub labels: HashMap<String, String>,
}

/// Every network on the host, the three Engine-created defaults included.
pub fn list_networks(cfg: &DockerConfig) -> Result<Vec<NetworkInfo>> {
    block_on(async {
        let docker = connect(cfg)?;
        let networks = docker
            .list_networks(None::<bollard::query_parameters::ListNetworksOptions>)
            .await
            .map_err(|e| friendly(cfg, e))?;
        Ok(networks.into_iter().map(network_info).collect())
    })
}

fn network_info(network: bollard::models::Network) -> NetworkInfo {
    NetworkInfo {
        id: network.id.unwrap_or_default(),
        name: network.name.unwrap_or_default(),
        driver: network.driver.unwrap_or_default(),
        scope: network.scope.unwrap_or_default(),
        internal: network.internal.unwrap_or(false),
        created: network.created.map(|d| d.unix_timestamp()),
        labels: network.labels.unwrap_or_default(),
    }
}

/// One category of `docker system df` (images, containers, volumes or build
/// cache): counts plus bytes, exactly as the Engine reports them.
#[derive(Default)]
pub struct DiskUsageGroup {
    pub active_count: i64,
    pub total_count: i64,
    pub size_bytes: u64,
    pub reclaimable_bytes: u64,
}

fn disk_usage_group(
    active_count: Option<i64>,
    total_count: Option<i64>,
    total_size: Option<i64>,
    reclaimable: Option<i64>,
) -> DiskUsageGroup {
    DiskUsageGroup {
        active_count: active_count.unwrap_or(0),
        total_count: total_count.unwrap_or(0),
        size_bytes: total_size.unwrap_or(0).max(0) as u64,
        reclaimable_bytes: reclaimable.unwrap_or(0).max(0) as u64,
    }
}

/// `docker system df`'s four categories. Deliberately cheap to call is not a
/// property this has — the Engine walks images, containers and volumes to
/// answer, so callers only ask for it when a user opens the Docker settings
/// section, never on a poll.
#[derive(Default)]
pub struct DiskUsage {
    pub images: DiskUsageGroup,
    pub containers: DiskUsageGroup,
    pub volumes: DiskUsageGroup,
    pub build_cache: DiskUsageGroup,
}

pub fn disk_usage(cfg: &DockerConfig) -> Result<DiskUsage> {
    block_on(async {
        let docker = connect(cfg)?;
        let df = docker
            .df(None::<bollard::query_parameters::DataUsageOptions>)
            .await
            .map_err(|e| friendly(cfg, e))?;
        let images = df.image_usage.unwrap_or_default();
        let containers = df.container_usage.unwrap_or_default();
        let volumes = df.volume_usage.unwrap_or_default();
        let build_cache = df.build_cache_usage.unwrap_or_default();
        Ok(DiskUsage {
            images: disk_usage_group(
                images.active_count,
                images.total_count,
                images.total_size,
                images.reclaimable,
            ),
            containers: disk_usage_group(
                containers.active_count,
                containers.total_count,
                containers.total_size,
                containers.reclaimable,
            ),
            volumes: disk_usage_group(
                volumes.active_count,
                volumes.total_count,
                volumes.total_size,
                volumes.reclaimable,
            ),
            build_cache: disk_usage_group(
                build_cache.active_count,
                build_cache.total_count,
                build_cache.total_size,
                build_cache.reclaimable,
            ),
        })
    })
}

/// One BuildKit cache record, from `docker system df`'s own per-record
/// listing — there is no dedicated "list build cache" endpoint, only this one
/// nested inside `df`.
pub struct BuildCacheEntry {
    pub id: String,
    /// Backing an in-progress or otherwise live build; never removed.
    pub in_use: bool,
    pub size_bytes: u64,
}

/// Every build cache record the Engine currently holds, for a prune's
/// dry-run: unlike images/volumes, cache records carry no ASC ownership to
/// protect — plain in-use is the whole story.
pub fn build_cache_entries(cfg: &DockerConfig) -> Result<Vec<BuildCacheEntry>> {
    block_on(async {
        let docker = connect(cfg)?;
        let df = docker
            .df(None::<bollard::query_parameters::DataUsageOptions>)
            .await
            .map_err(|e| friendly(cfg, e))?;
        let items = df
            .build_cache_usage
            .and_then(|u| u.items)
            .unwrap_or_default();
        Ok(items
            .into_iter()
            .filter_map(|value| serde_json::from_value::<bollard::models::BuildCache>(value).ok())
            .map(|entry| BuildCacheEntry {
                id: entry.id.unwrap_or_default(),
                in_use: entry.in_use.unwrap_or(false),
                size_bytes: entry.size.unwrap_or(0).max(0) as u64,
            })
            .collect())
    })
}

/// Prune the whole build cache in one Engine call (`docker builder prune`).
/// Unlike images/volumes, there is no per-record delete endpoint and no ASC
/// ownership to protect here, so a bulk call is the Engine's only option and
/// carries none of the risk the images/volumes' one-at-a-time removal guards
/// against. Returns the ids removed and bytes reclaimed.
pub fn prune_build_cache(cfg: &DockerConfig) -> Result<(Vec<String>, u64)> {
    block_on(async {
        let docker = connect(cfg)?;
        let result = docker
            .prune_build(None::<bollard::query_parameters::PruneBuildOptions>)
            .await
            .map_err(|e| friendly(cfg, e))?;
        Ok((
            result.caches_deleted.unwrap_or_default(),
            result.space_reclaimed.unwrap_or(0).max(0) as u64,
        ))
    })
}

/// One-shot resource counters of a container, straight off the Engine's
/// stats endpoint.
pub struct ContainerUsage {
    /// Cumulative CPU time, microseconds.
    pub cpu_time_micros: u64,
    /// Resident memory, bytes.
    pub memory_bytes: u64,
    /// The container's memory limit, bytes. `None` when it has none — which
    /// is the common case, and why the caller must not render it as 0.
    pub memory_limit_bytes: Option<u64>,
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
                let memory = stats.memory_stats.unwrap_or_default();
                let Some(memory_bytes) = memory.usage else {
                    return Ok(None);
                };
                let memory_limit_bytes = memory.limit;
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
                    memory_limit_bytes,
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
pub fn ensure_pulled(
    cfg: &DockerConfig,
    image: &str,
    auth: Option<&RegistryAuth>,
    report: Option<&dyn progress::InstallReporter>,
) -> Result<()> {
    block_on(async {
        let docker = connect(cfg)?;
        match docker.inspect_image(image).await {
            Ok(_) => Ok(()),
            Err(e) if status_of(&e) == Some(404) => pull(&docker, image, auth, report)
                .await
                .map_err(|e| anyhow!("{}: {e}", tf(Msg::ErrImagePull, image))),
            Err(e) => Err(friendly(cfg, e)),
        }
    })
}

/// What the host knows about one local image (DMN-120).
#[derive(Debug, Clone)]
pub struct LocalImage {
    /// `sha256:…` image id.
    pub id: String,
    /// `repo@sha256:…` — the registry manifest digests this image was
    /// pulled as; empty for a locally built image.
    pub repo_digests: Vec<String>,
    /// Unix seconds.
    pub created: Option<i64>,
    pub labels: HashMap<String, String>,
}

/// Inspect a local image. `None` when it is not on the host (404).
pub fn inspect_local_image(cfg: &DockerConfig, image: &str) -> Result<Option<LocalImage>> {
    block_on(async {
        let docker = connect(cfg)?;
        match docker.inspect_image(image).await {
            Ok(info) => Ok(Some(LocalImage {
                id: info.id.unwrap_or_default(),
                repo_digests: info.repo_digests.unwrap_or_default(),
                created: info.created.map(|d| d.unix_timestamp()),
                labels: info.config.and_then(|c| c.labels).unwrap_or_default(),
            })),
            Err(e) if status_of(&e) == Some(404) => Ok(None),
            Err(e) => Err(friendly(cfg, e)),
        }
    })
}

/// How long [`registry_digest`] may wait on a registry: it is a UI status
/// probe, and a slow or unreachable registry must not hang the page.
const REGISTRY_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// The manifest digest `image` (a tag reference) currently resolves to on
/// its registry — `GET /distribution/{name}/json`, no layers downloaded.
/// For a multi-arch image this is the index digest, the same value the
/// Engine records in `RepoDigests` after a pull by tag, so the two compare
/// directly.
pub fn registry_digest(
    cfg: &DockerConfig,
    image: &str,
    auth: Option<&RegistryAuth>,
) -> Result<String> {
    block_on(async {
        let docker = connect(cfg)?;
        let (from_image, tag) = image_ref(image);
        let reference = match tag {
            Some(tag) => format!("{from_image}:{tag}"),
            None => from_image.to_string(),
        };
        let probe =
            docker.inspect_registry_image(&reference, auth.map(RegistryAuth::to_credentials));
        match tokio::time::timeout(REGISTRY_PROBE_TIMEOUT, probe).await {
            Ok(Ok(info)) => info
                .descriptor
                .digest
                .ok_or_else(|| anyhow!("registry returned no digest for {image}")),
            Ok(Err(e)) => Err(friendly(cfg, e)),
            Err(_) => Err(anyhow!("registry did not answer for {image} in time")),
        }
    })
}

/// Pull `image` unconditionally — unlike [`ensure_pulled`], an image already
/// on the host is refreshed from its registry (`docker pull`). Used by the
/// repull of a mutable tag (DMN-120).
pub fn pull_image(cfg: &DockerConfig, image: &str, auth: Option<&RegistryAuth>) -> Result<()> {
    block_on(async {
        let docker = connect(cfg)?;
        pull(&docker, image, auth, None)
            .await
            .map_err(|e| anyhow!("{}: {e}", tf(Msg::ErrImagePull, image)))
    })
}

/// The id of the image a container was created from; `None` when the
/// container does not exist.
pub fn container_image_id(cfg: &DockerConfig, container: &str) -> Result<Option<String>> {
    block_on(async {
        let docker = connect(cfg)?;
        match docker.inspect_container(container, None).await {
            Ok(info) => Ok(info.image),
            Err(e) if status_of(&e) == Some(404) => Ok(None),
            Err(e) => Err(friendly(cfg, e)),
        }
    })
}

/// Split a reference into `(repository, tag)` the way the Engine does —
/// `None` tag for a digest reference; a bare name gets `latest`.
pub fn split_image_ref(image: &str) -> (&str, Option<&str>) {
    image_ref(image)
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
pub fn logs_tail(
    cfg: &DockerConfig,
    container: &str,
    tail: usize,
    timestamps: bool,
) -> Result<String> {
    block_on(async {
        let docker = connect(cfg)?;
        let opts = LogsOptions {
            stdout: true,
            stderr: true,
            follow: false,
            timestamps,
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
    /// Container labels (DMN-105): `asc.managed`/`asc.app.id`/`asc.app.uuid`,
    /// see the constants below. Purely informational — [`AppliedConfig`]
    /// deliberately does not read labels back, so adding or changing one here
    /// never trips the settings-drift recreate.
    pub labels: HashMap<String, String>,
}

/// Set to `"true"` on every container ASC creates (DMN-105): lets a future
/// caller recognize an ASC-managed container without guessing from its name.
pub const LABEL_MANAGED: &str = "asc.managed";
/// The installed app's `id` (DMN-105) — a cheaper cross-reference than
/// matching on container name, kept alongside it rather than instead of it.
pub const LABEL_APP_ID: &str = "asc.app.id";
/// The installed app's `uuid` (DMN-105), when it has one (DMN-044+).
pub const LABEL_APP_UUID: &str = "asc.app.uuid";

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
    report: Option<&dyn progress::InstallReporter>,
) -> std::result::Result<(), BollardError> {
    let (from_image, tag) = image_ref(image);
    let opts = CreateImageOptions {
        from_image: Some(from_image.to_string()),
        tag: tag.map(str::to_string),
        ..Default::default()
    };
    let mut bars = progress::interactive().then(progress::LayerBars::new);
    if let Some(report) = report {
        report.line(&format!("Pulling image {image}"));
    }
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
        if let Some(report) = report {
            let line = if layer.is_empty() {
                status.to_string()
            } else {
                let progress = bytes
                    .map(|(current, total)| format!(" {current}/{total}"))
                    .unwrap_or_default();
                format!("{layer}: {status}{progress}")
            };
            if !line.trim().is_empty() {
                report.line(&line);
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
pub fn build_image(
    cfg: &DockerConfig,
    spec: BuildSpec<'_>,
    report: Option<&dyn progress::InstallReporter>,
) -> Result<()> {
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
    if let Some(report) = report {
        report.line(&format!(
            "Building image {} ({})",
            spec.tag, spec.dockerfile
        ));
    }
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
                    build_trace(spec.tag, trace, bars.as_mut(), report);
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
fn build_trace(
    tag: &str,
    trace: &StatusResponse,
    mut bars: Option<&mut progress::BuildBars>,
    report: Option<&dyn progress::InstallReporter>,
) {
    for vertex in &trace.vertexes {
        // A vertex is announced before it runs; docker shows nothing for it
        // until it starts, and neither do we.
        let Some(state) = vertex_state(vertex) else {
            continue;
        };
        match &state {
            progress::StepState::Running => {
                debug!(tag, step = vertex.name, "running");
                if let Some(report) = report {
                    report.line(&format!("{}: running", vertex.name));
                }
            }
            terminal => {
                info!(tag, step = vertex.name, "{}", terminal.label());
                if let Some(report) = report {
                    report.line(&format!("{}: {}", vertex.name, terminal.label()));
                }
            }
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
            labels: (!spec.labels.is_empty()).then(|| spec.labels.clone()),
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
                pull(&docker, spec.image, spec.registry_auth.as_ref(), None)
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

/// When the container's current run began — or, once it stopped, its most
/// recent one. The console's cut-off between this run's output and every
/// earlier run's (DMN-116): a `docker start` of an existing container
/// appends to the same log the Engine has kept since the container was
/// created, so neither `tail` nor attach's `logs` replay can tell the runs
/// apart on their own. `None` when the container is missing or has never
/// started (the Engine's zero `"0001-01-01T00:00:00Z"`).
pub async fn run_started_at(
    cfg: &DockerConfig,
    container: &str,
) -> Result<Option<time::OffsetDateTime>> {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    let docker = connect(cfg)?;
    match docker.inspect_container(container, None).await {
        Ok(info) => Ok(info
            .state
            .and_then(|s| s.started_at)
            .and_then(|raw| OffsetDateTime::parse(&raw, &Rfc3339).ok())
            .filter(|started| started.unix_timestamp() > 0)),
        Err(e) if status_of(&e) == Some(404) => Ok(None),
        Err(e) => Err(friendly(cfg, e)),
    }
}

/// Splits the Engine's `timestamps=true` prefix (RFC3339 with nanoseconds,
/// then one space) off a log message. `None` when the message does not
/// start with one — the caller keeps such a message rather than guessing
/// which run it belongs to.
fn split_log_timestamp(message: &[u8]) -> Option<(time::OffsetDateTime, &[u8])> {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    let space = message.iter().position(|&b| b == b' ')?;
    let stamp = std::str::from_utf8(&message[..space]).ok()?;
    let parsed = OffsetDateTime::parse(stamp, &Rfc3339).ok()?;
    Some((parsed, &message[space + 1..]))
}

/// The Engine's `since` is whole seconds, so it only narrows the transfer:
/// the exact cut is made on each message's own nanosecond timestamp — a
/// restart's shutdown lines of the previous run usually land within the
/// same second as the new `StartedAt`.
fn since_seconds(since: Option<time::OffsetDateTime>) -> i32 {
    since
        .map(|at| i32::try_from(at.unix_timestamp()).unwrap_or(0))
        .unwrap_or(0)
}

/// Follow-mode logs as a stream of UTF-8 text lines (trailing newline
/// stripped). Timestamps are included by the Engine. With `since`, lines
/// logged before that moment are dropped — the console passes the
/// container's [`run_started_at`] so it shows the current run only.
pub async fn logs_follow(
    cfg: &DockerConfig,
    container: &str,
    tail: usize,
    since: Option<time::OffsetDateTime>,
) -> Result<impl Stream<Item = Result<String>> + Send> {
    let docker = connect(cfg)?;
    let opts = LogsOptions {
        follow: true,
        stdout: true,
        stderr: true,
        timestamps: true,
        tail: tail.to_string(),
        since: since_seconds(since),
        ..Default::default()
    };
    // The stream owns its transport handle, so `docker` may drop here.
    let stream = docker
        .logs(container, Some(opts))
        .filter(move |item| {
            let keep = match (item, since) {
                (Ok(log), Some(since)) => split_log_timestamp(log.as_ref())
                    .is_none_or(|(logged_at, _)| logged_at >= since),
                _ => true,
            };
            std::future::ready(keep)
        })
        .map(|item| {
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

/// What the container printed in `[since, until)`, oldest first, capped to
/// the last `tail` messages, with the Engine's timestamps stripped again —
/// the raw bytes an attach would have carried. This is the attach console's
/// backlog (DMN-116): `until` is the moment the live attach went up, so the
/// backlog and the live stream meet there instead of overlapping.
pub async fn run_backlog(
    cfg: &DockerConfig,
    container: &str,
    since: time::OffsetDateTime,
    until: time::OffsetDateTime,
    tail: usize,
) -> Result<Vec<Vec<u8>>> {
    let docker = connect(cfg)?;
    let opts = LogsOptions {
        follow: false,
        stdout: true,
        stderr: true,
        timestamps: true,
        tail: tail.to_string(),
        since: since_seconds(Some(since)),
        ..Default::default()
    };
    let mut stream = docker.logs(container, Some(opts));
    let mut backlog = Vec::new();
    while let Some(item) = stream.next().await {
        let log = match item {
            Ok(log) => log,
            // Removed between inspect and here: nothing to replay.
            Err(e) if status_of(&e) == Some(404) => break,
            Err(e) => return Err(friendly(cfg, e)),
        };
        let bytes = log.into_bytes();
        match split_log_timestamp(&bytes) {
            Some((logged_at, message)) if logged_at >= since && logged_at < until => {
                backlog.push(message.to_vec())
            }
            Some(_) => {}
            None => backlog.push(bytes.to_vec()),
        }
    }
    Ok(backlog)
}

/// Interactive attach: bidirectional stdin/stdout to a running container.
///
/// `replay_logs` asks the Engine to send the container's buffered output
/// before switching to live streaming. The WS console hub needs this: a
/// bare attach only carries output produced after it connects, so the very
/// first attach of a freshly (re)created hub session (`console::hub::spawn`)
/// would otherwise miss everything the app printed before that call —
/// including its own startup banner — with no way to recover it later, since
/// the hub's own replay buffer only starts accumulating from that point. The
/// interactive CLI attach (`asc app attach`) passes `false` to match
/// `docker attach`'s own convention of showing only new output.
pub async fn attach(
    cfg: &DockerConfig,
    container: &str,
    replay_logs: bool,
) -> Result<AttachContainerResults> {
    let docker = connect(cfg)?;
    let opts = AttachContainerOptions {
        stdin: true,
        stdout: true,
        stderr: true,
        stream: true,
        logs: replay_logs,
        detach_keys: None,
    };
    docker
        .attach_container(container, Some(opts))
        .await
        .map_err(|e| friendly(cfg, e))
}

/// The resize half of an exec session, split out from [`ExecSession`] itself
/// so calling it does not need `&ExecSession`. `ExecSession` holds trait
/// objects (`output`/`input`) that are `Send` but not `Sync`, which makes
/// `&ExecSession` not `Send` — and the WebSocket handler this feeds is
/// required to be `Send` end to end (it runs on axum's `on_upgrade`, see
/// ws.rs). `Docker` and the exec id are plain `Send + Sync` data, so a
/// reference to just this struct carries across an `.await` without issue.
#[derive(Clone)]
pub struct ExecResizer {
    docker: Docker,
    exec_id: String,
}

impl ExecResizer {
    /// Resizes the exec's PTY. Best-effort by design at the call sites: a
    /// resize failing (the process already exited, e.g.) should not tear
    /// down an otherwise-live session.
    pub async fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.docker
            .resize_exec(
                &self.exec_id,
                ResizeExecOptions {
                    width: cols,
                    height: rows,
                },
            )
            .await
            .map_err(|e| anyhow!("resize exec: {e}"))
    }
}

/// A live `docker exec` session (DMN-082): bidirectional stdin/stdout over a
/// PTY, plus resize via `resizer`. Kept open for the life of the shell.
pub struct ExecSession {
    pub resizer: ExecResizer,
    pub output: Pin<Box<dyn Stream<Item = std::result::Result<LogOutput, BollardError>> + Send>>,
    pub input: Pin<Box<dyn AsyncWrite + Send>>,
}

/// Shells to try when the caller does not name a command, in order.
const SHELL_PROBE: [&str; 2] = ["/bin/bash", "/bin/sh"];

/// Interactive shell inside a running container: `docker exec` with a PTY.
/// An empty `command` probes `SHELL_PROBE` in order and uses the first one
/// that actually runs.
///
/// The Engine API does **not** fail `create_exec`/`start_exec` for a missing
/// interpreter: the runtime error ("OCI runtime exec failed: ... no such
/// file or directory") arrives as *output* of an otherwise successfully
/// started exec, not as an `Err` from either call — confirmed against a real
/// `busybox` container in `tests/exec_docker.rs`, which has `/bin/sh` but
/// not `/bin/bash`. So the probe cannot try to start each candidate
/// interactively and catch a `Result::Err`; instead each candidate is
/// verified first with a short-lived, non-interactive `<shell> -c "exit 0"`
/// whose exit code is checked via `inspect_exec`, and only the winner gets
/// the real interactive (PTY) exec.
pub async fn exec(
    cfg: &DockerConfig,
    container: &str,
    command: &[String],
    cols: u16,
    rows: u16,
) -> Result<ExecSession> {
    let docker = connect(cfg)?;

    let cmd = if command.is_empty() {
        probe_shell(&docker, cfg, container).await?
    } else {
        command.to_vec()
    };

    let (exec_id, output, input) = start_exec(&docker, cfg, container, &cmd, true).await?;
    let resizer = ExecResizer { docker, exec_id };
    // Best effort: the shell already has a default TTY size from the
    // Engine, and a client's own resize frame corrects it a moment later
    // regardless.
    let _ = resizer.resize(cols, rows).await;
    Ok(ExecSession {
        resizer,
        output,
        input,
    })
}

/// Finds the first of `SHELL_PROBE` that actually runs in `container` — see
/// [`exec`] for why a synchronous `Err` from `start_exec` cannot be trusted
/// to catch a missing interpreter.
async fn probe_shell(docker: &Docker, cfg: &DockerConfig, container: &str) -> Result<Vec<String>> {
    for shell in SHELL_PROBE {
        let probe = vec![shell.to_string(), "-c".to_string(), "exit 0".to_string()];
        if shell_probe_succeeds(docker, cfg, container, &probe).await {
            return Ok(vec![shell.to_string()]);
        }
    }
    Err(anyhow!(
        "no shell found in the container (tried {})",
        SHELL_PROBE.join(", ")
    ))
}

/// Runs `probe` to completion (non-interactive, no PTY) and reports whether
/// it exited 0. Any failure along the way — starting the exec, or reading
/// its exit code back — counts as "no": the caller falls through to the
/// next candidate rather than surfacing a probe-only failure to the user.
async fn shell_probe_succeeds(
    docker: &Docker,
    cfg: &DockerConfig,
    container: &str,
    probe: &[String],
) -> bool {
    let Ok((exec_id, mut output, _input)) = start_exec(docker, cfg, container, probe, false).await
    else {
        return false;
    };
    // Docker closes this stream once the process exits; draining it is how
    // we wait for the exit code to become available on inspect.
    while output.next().await.is_some() {}
    matches!(
        docker.inspect_exec(&exec_id).await,
        Ok(inspect) if inspect.exit_code == Some(0)
    )
}

type ExecStreams = (
    String,
    Pin<Box<dyn Stream<Item = std::result::Result<LogOutput, BollardError>> + Send>>,
    Pin<Box<dyn AsyncWrite + Send>>,
);

/// `tty = false` is used for the non-interactive `probe`; `true` for the
/// real interactive shell the caller ends up talking to.
async fn start_exec(
    docker: &Docker,
    cfg: &DockerConfig,
    container: &str,
    cmd: &[String],
    tty: bool,
) -> Result<ExecStreams> {
    let created = docker
        .create_exec(
            container,
            CreateExecOptions {
                attach_stdin: Some(tty),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                tty: Some(tty),
                cmd: Some(cmd.to_vec()),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| friendly(cfg, e))?;

    let started = docker
        .start_exec(
            &created.id,
            Some(StartExecOptions {
                tty,
                ..Default::default()
            }),
        )
        .await
        .map_err(|e| friendly(cfg, e))?;

    match started {
        StartExecResults::Attached { output, input } => Ok((created.id, output, input)),
        StartExecResults::Detached => Err(anyhow!(
            "exec started detached; expected an attached session"
        )),
    }
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
