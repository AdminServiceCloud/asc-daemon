//! Daemon API (DMN-005): gRPC (tonic; wire-compatible with the platform's
//! ConnectRPC clients) and REST (JSON) served **together on one listener**,
//! both calling the same service layer and sharing bearer-token auth.
//!
//! Remote access goes through the platform tunnel; locally the API listens
//! on localhost only (config `[api] listen`).

pub mod console;
mod grpc;
mod local;
pub mod proto;
mod rest;
pub mod tls;
pub mod tokens;
pub mod uds;
mod ws;

use std::future::Future;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use tracing::{debug, info, warn};

use crate::daemon::apps::meta::AppMeta;
use crate::daemon::apps::{AppManager, AppStatus, Outcome, RuntimeState, UserContext};
use crate::daemon::config::Config;
use crate::daemon::docker;
use crate::daemon::files;
use crate::daemon::i18n::{Msg, t, tf};
use crate::daemon::monitor::Monitor;
use crate::daemon::pkg;
use crate::daemon::progress;
use crate::daemon::users;

use console::ConsoleTokens;
use tokens::TokenStore;

/// Optional features this daemon build supports, reported by `GetStatus`
/// (DMN-076) so a caller can gate UI on what actually works instead of
/// discovering it by hitting `UNIMPLEMENTED`. Append a value here in the same
/// change that ships the matching capability; never remove or rename a value
/// once released — older platform builds may still be checking for it.
pub const CAPABILITIES: &[&str] = &[
    "sources",
    "credentials",
    "ssh-credentials",
    "console.exec",
    "app.stats",
    "app.ports",
    "app.uptime",
    "users",
    "docker.containers",
    "docker.stats",
    "ports.listening",
    "docker.inventory",
    "docker.prune",
];

/// Shared state behind both transports.
pub struct ApiState {
    pub config: Config,
    pub manager: AppManager,
    pub console_tokens: ConsoleTokens,
    /// Shared attach sessions: one source per app, many console clients.
    pub attach_hub: crate::daemon::console::hub::AttachHub,
    /// System metrics ring buffer, filled by the daemon's sampler task.
    pub monitor: Arc<Monitor>,
    /// The bearer tokens this daemon accepts: the long-lived primary and the
    /// short-lived access tokens minted from it (DMN-065).
    pub tokens: TokenStore,
}

/// Apps-wide disk report (`asc disk` with no app): what each app occupies,
/// against the capacity of the filesystem the app store lives on.
pub struct DiskSummary {
    /// `None` when the filesystem cannot be queried (statvfs failure).
    pub fs_total: Option<u64>,
    /// Largest first.
    pub apps: Vec<AppDiskRow>,
}

pub struct AppDiskRow {
    pub id: String,
    /// The name shown to the user: their custom name, else the package title.
    pub name: String,
    pub owner: String,
    pub bytes: u64,
}

pub struct AppPortsRow {
    pub id: String,
    pub name: String,
    pub owner: String,
    pub ports: Vec<crate::daemon::docker::PublishedPort>,
}

/// Milliseconds between the two readings a CPU percentage is derived from.
/// The same window `AppManager::stats_for` uses, so `asc docker stats` and
/// `asc stats` are directly comparable.
const STATS_SAMPLE_MILLIS: u64 = 500;

/// One container of [`ApiState::list_containers`], with the ASC app it
/// belongs to resolved (if any).
pub struct ContainerRow {
    pub info: docker::ContainerInfo,
    /// Id of the installed app whose runtime is this container.
    pub app_id: Option<String>,
    /// That app's stable uuid; absent for apps installed before DMN-044.
    pub app_uuid: Option<String>,
}

/// One container's live resource usage — see
/// [`ApiState::list_container_stats`].
pub struct ContainerStatsRow {
    pub id: String,
    /// Percent of one host CPU; can exceed 100 on a multi-core host.
    pub cpu_percent: f64,
    pub memory_bytes: u64,
    /// `None` when the container has no memory limit — the common case, and
    /// why a caller must not render it as 0.
    pub memory_limit_bytes: Option<u64>,
    pub net_rx_bytes: Option<u64>,
    pub net_tx_bytes: Option<u64>,
    pub block_read_bytes: Option<u64>,
    pub block_write_bytes: Option<u64>,
}

/// One counter reading of a container, or `None` with a warning logged.
///
/// A container that vanished mid-sample, or an Engine hiccup on one
/// container out of fifty, must not fail the whole listing: the row is
/// simply dropped further up.
fn usage_or_warn(
    cfg: &crate::daemon::config::DockerConfig,
    id: &str,
) -> Option<docker::ContainerUsage> {
    match docker::stats_usage(cfg, id) {
        Ok(usage) => usage,
        Err(err) => {
            warn!(container = %id, error = %format!("{err:#}"), "cannot query container stats");
            None
        }
    }
}

/// CPU percentage from two cumulative readings over a wall-clock interval —
/// the same arithmetic `apps::cpu_percent` does for apps.
fn cpu_percent_between(
    first: &docker::ContainerUsage,
    second: &docker::ContainerUsage,
    elapsed_micros: u64,
) -> f64 {
    if elapsed_micros == 0 {
        return 0.0;
    }
    let delta = second.cpu_time_micros.saturating_sub(first.cpu_time_micros);
    delta as f64 / elapsed_micros as f64 * 100.0
}

// ── Host inventory & cleanup (DMN-104/DMN-105) ──────────────────────────────

/// One row of [`ApiState::list_images`].
pub struct DockerImageRow {
    pub id: String,
    pub tags: Vec<String>,
    pub size_bytes: u64,
    pub created: i64,
    pub labels: std::collections::HashMap<String, String>,
    pub dangling: bool,
    /// An installed app (running or not) still runs this image.
    pub asc_protected: bool,
    /// Set together with `asc_protected`: which app, for the UI to explain
    /// the lock without a separate lookup.
    pub protected_reason: Option<String>,
}

/// One row of [`ApiState::list_volumes`].
pub struct DockerVolumeRow {
    pub name: String,
    pub driver: String,
    pub mountpoint: String,
    pub created_at: Option<i64>,
    pub labels: std::collections::HashMap<String, String>,
    pub ref_count: Option<i32>,
    pub size_bytes: Option<u64>,
    pub asc_protected: bool,
    pub protected_reason: Option<String>,
}

/// One row of [`ApiState::list_networks`]. Inventory-only — see
/// [`ApiState::prune_docker`] for why networks are never a prune target.
pub struct DockerNetworkRow {
    pub id: String,
    pub name: String,
    pub driver: String,
    pub scope: String,
    pub internal: bool,
    pub created: Option<i64>,
    pub labels: std::collections::HashMap<String, String>,
}

/// One `docker system df` category.
#[derive(Default)]
pub struct DiskUsageGroupRow {
    pub active_count: i64,
    pub total_count: i64,
    pub size_bytes: u64,
    pub reclaimable_bytes: u64,
}

impl From<docker::DiskUsageGroup> for DiskUsageGroupRow {
    fn from(group: docker::DiskUsageGroup) -> Self {
        Self {
            active_count: group.active_count,
            total_count: group.total_count,
            size_bytes: group.size_bytes,
            reclaimable_bytes: group.reclaimable_bytes,
        }
    }
}

/// Result of [`ApiState::docker_disk_usage`].
pub struct DockerDiskUsageRow {
    pub images: DiskUsageGroupRow,
    pub containers: DiskUsageGroupRow,
    pub volumes: DiskUsageGroupRow,
    pub build_cache: DiskUsageGroupRow,
}

/// A [`ApiState::prune_docker`] target — mirrors the proto enum without the
/// `UNSPECIFIED` variant, which the gRPC/REST layers reject before this ever
/// sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneTarget {
    Images,
    Volumes,
    BuildCache,
}

/// One item [`ApiState::prune_docker`] considered but did not remove.
pub struct PruneSkipRow {
    pub name: String,
    pub reason: String,
}

/// Result of [`ApiState::prune_docker`], real run or dry run alike.
pub struct PruneDockerRow {
    pub removed: Vec<String>,
    pub reclaimed_bytes: u64,
    pub skipped: Vec<PruneSkipRow>,
}

/// Images and named volumes that [`ApiState::list_images`]/
/// [`ApiState::list_volumes`]/[`ApiState::prune_docker`] must never remove
/// (DMN-105): every installed app's currently effective image, and every
/// named volume its settings declare — regardless of whether the app is
/// running. Protection matches on the exact reference string the app's
/// manifest names (what [`docker::create`] actually passed to the Engine),
/// which is why it can never diverge from what was actually run.
struct DockerProtection {
    /// image reference -> reason.
    images: std::collections::HashMap<String, String>,
    /// volume name -> reason.
    volumes: std::collections::HashMap<String, String>,
}

impl DockerProtection {
    fn scan(state: &ApiState, ctx: &UserContext) -> Result<Self> {
        let mut images = std::collections::HashMap::new();
        let mut volumes = std::collections::HashMap::new();
        for app in state.manager.list(ctx)? {
            let app_dir = state.manager.store().app_dir(&app.meta.id)?;
            let footprint = pkg::docker_footprint(&state.config, &app.meta, &app_dir);
            let label = app.meta.display_name().to_string();
            if let Some(image) = footprint.image {
                images
                    .entry(image)
                    .or_insert_with(|| tf(Msg::DockerPruneProtectedByApp, &label));
            }
            for volume in footprint.named_volumes {
                volumes
                    .entry(volume)
                    .or_insert_with(|| tf(Msg::DockerPruneProtectedByApp, &label));
            }
        }
        Ok(Self { images, volumes })
    }

    fn image_reason(&self, tags: &[String], id: &str) -> Option<&str> {
        tags.iter()
            .find_map(|tag| self.images.get(tag))
            .or_else(|| self.images.get(id))
            .map(String::as_str)
    }

    fn volume_reason(&self, name: &str) -> Option<&str> {
        self.volumes.get(name).map(String::as_str)
    }
}

/// First 12 characters of an image id, its `sha256:` prefix stripped — the
/// same abbreviation `docker images` shows.
fn short_image_id(id: &str) -> String {
    id.trim_start_matches("sha256:").chars().take(12).collect()
}

/// [`ApiState::prune_docker`]'s `PruneTarget::Images` branch.
fn prune_images(
    state: &ApiState,
    ctx: &UserContext,
    dry_run: bool,
    dangling_only: bool,
) -> Result<PruneDockerRow> {
    let images = docker::list_images(&state.config.docker)?;
    let protection = DockerProtection::scan(state, ctx)?;
    let mut removed = Vec::new();
    let mut skipped = Vec::new();
    let mut reclaimed_bytes = 0u64;
    for image in images {
        if dangling_only && !image.dangling {
            continue;
        }
        let label = image
            .tags
            .first()
            .cloned()
            .unwrap_or_else(|| short_image_id(&image.id));
        if let Some(reason) = protection.image_reason(&image.tags, &image.id) {
            skipped.push(PruneSkipRow {
                name: label,
                reason: reason.to_string(),
            });
            continue;
        }
        if dry_run {
            removed.push(label);
            reclaimed_bytes += image.size;
            continue;
        }
        match docker::remove_image(&state.config.docker, &image.id) {
            Ok(()) => {
                removed.push(label);
                reclaimed_bytes += image.size;
            }
            Err(err) => skipped.push(PruneSkipRow {
                name: label,
                reason: format!("{err:#}"),
            }),
        }
    }
    Ok(PruneDockerRow {
        removed,
        reclaimed_bytes,
        skipped,
    })
}

/// [`ApiState::prune_docker`]'s `PruneTarget::Volumes` branch.
fn prune_volumes(state: &ApiState, ctx: &UserContext, dry_run: bool) -> Result<PruneDockerRow> {
    let volumes = docker::list_volumes(&state.config.docker)?;
    let protection = DockerProtection::scan(state, ctx)?;
    let mut removed = Vec::new();
    let mut skipped = Vec::new();
    let mut reclaimed_bytes = 0u64;
    for volume in volumes {
        if let Some(reason) = protection.volume_reason(&volume.name) {
            skipped.push(PruneSkipRow {
                name: volume.name,
                reason: reason.to_string(),
            });
            continue;
        }
        let size = volume.size_bytes.unwrap_or(0);
        if dry_run {
            removed.push(volume.name);
            reclaimed_bytes += size;
            continue;
        }
        match docker::remove_volume(&state.config.docker, &volume.name) {
            Ok(()) => {
                removed.push(volume.name);
                reclaimed_bytes += size;
            }
            Err(err) => skipped.push(PruneSkipRow {
                name: volume.name,
                reason: format!("{err:#}"),
            }),
        }
    }
    Ok(PruneDockerRow {
        removed,
        reclaimed_bytes,
        skipped,
    })
}

/// [`ApiState::prune_docker`]'s `PruneTarget::BuildCache` branch. No ASC
/// ownership applies here (see the module doc on [`docker::prune_build_cache`]),
/// so a dry run just reports what is not currently in use, and a real run is
/// the Engine's own bulk prune rather than a per-record loop.
fn prune_build_cache_target(state: &ApiState, dry_run: bool) -> Result<PruneDockerRow> {
    if !dry_run {
        let (removed, reclaimed_bytes) = docker::prune_build_cache(&state.config.docker)?;
        return Ok(PruneDockerRow {
            removed,
            reclaimed_bytes,
            skipped: Vec::new(),
        });
    }
    let entries = docker::build_cache_entries(&state.config.docker)?;
    let mut removed = Vec::new();
    let mut skipped = Vec::new();
    let mut reclaimed_bytes = 0u64;
    for entry in entries {
        if entry.in_use {
            skipped.push(PruneSkipRow {
                name: entry.id,
                reason: t(Msg::DockerBuildCacheInUse).to_string(),
            });
            continue;
        }
        reclaimed_bytes += entry.size_bytes;
        removed.push(entry.id);
    }
    Ok(PruneDockerRow {
        removed,
        reclaimed_bytes,
        skipped,
    })
}

/// One row of [`ApiState::listening_ports`] (DMN-103): a real host socket,
/// or a declared-but-unbound port of a stopped app.
pub struct ListeningPortRow {
    pub port: u16,
    /// "tcp" or "udp".
    pub protocol: &'static str,
    pub address: String,
    /// "ipv4" or "ipv6".
    pub family: &'static str,
    pub pid: Option<u32>,
    pub process: Option<String>,
    pub command: Option<String>,
    pub container_id: Option<String>,
    pub container_name: Option<String>,
    pub app_id: Option<String>,
    pub app_uuid: Option<String>,
    /// Declared by an installed app's settings but nothing is bound right
    /// now — the app is stopped. Never set together with a container or pid.
    pub declared_only: bool,
    /// This daemon's own API listener ([api] listen / acme_http_listen).
    pub is_daemon: bool,
}

/// Port number out of a `"host:port"` or `"[host]:port"` listen address —
/// the shape every `[api]` listen field uses. `None` for a malformed value,
/// which simply means nothing in the response gets flagged `is_daemon`.
fn listen_port(address: &str) -> Option<u16> {
    address.rsplit_once(':')?.1.parse().ok()
}

/// `(container id, first name, owning app id, owning app uuid)`.
type ContainerPortOwner = (String, String, Option<String>, Option<String>);

/// One container's published ports, keyed by `(host_port, transport)` — the
/// same shape docker port cross-referencing needs, built once per
/// [`ApiState::listening_ports`] call rather than per socket.
struct ContainerPortIndex {
    /// (port, "tcp"|"udp") -> (container id, first name, owning app).
    by_port: std::collections::HashMap<(u16, &'static str), ContainerPortOwner>,
}

impl ContainerPortIndex {
    /// `owners` is container name -> (app id, app uuid), the same map
    /// [`ApiState::container_owners`] builds for [`ApiState::list_containers`] —
    /// `docker::ContainerInfo` itself carries no app identity, only names.
    fn build(
        containers: &[docker::ContainerInfo],
        owners: &std::collections::HashMap<String, (String, Option<String>)>,
    ) -> Self {
        let mut by_port = std::collections::HashMap::new();
        for container in containers {
            let name = container
                .names
                .first()
                .cloned()
                .unwrap_or_else(|| container.id.clone());
            let owner = container
                .names
                .iter()
                .find_map(|n| owners.get(n.as_str()))
                .cloned();
            let (app_id, app_uuid) = match owner {
                Some((app_id, app_uuid)) => (Some(app_id), app_uuid),
                None => (None, None),
            };
            for port in &container.ports {
                let Some(public) = port.public else { continue };
                let transport: &'static str = if port.protocol == "udp" { "udp" } else { "tcp" };
                by_port.entry((public, transport)).or_insert_with(|| {
                    (
                        container.id.clone(),
                        name.clone(),
                        app_id.clone(),
                        app_uuid.clone(),
                    )
                });
            }
        }
        Self { by_port }
    }

    fn lookup(&self, port: u16, protocol: &'static str) -> Option<&ContainerPortOwner> {
        self.by_port.get(&(port, protocol))
    }
}

/// One event of [`ApiState::install_stream`]: a progress line, or the
/// terminal result — the same `Result` the unary [`ApiState::install`]
/// returns.
pub enum InstallStreamEvent {
    Line(String),
    Done(Result<pkg::InstallOutcome>),
}

/// One event of [`ApiState::upgrade_stream`]: a progress line, or the
/// terminal result — the same `Result` the unary [`ApiState::upgrade`]
/// returns.
pub enum UpgradeStreamEvent {
    Line(String),
    Done(Result<pkg::UpgradeOutcome>),
}

/// Send one progress line from the worker that produced it, whichever
/// context that worker happens to be in.
///
/// The install/upgrade worker is a `spawn_blocking` thread, where
/// `blocking_send` is the right call: a stream that outruns its reader
/// waits for it instead of losing progress. Part of that progress, though,
/// is reported from inside [`crate::daemon::docker::block_on`]'s
/// current-thread runtime — an image pull or a BuildKit build reports layer
/// by layer from within the async stream, on this very thread — and
/// `blocking_send` panics outright when called from a runtime thread. That
/// panic took the worker, its sender and the whole stream with it, leaving
/// the platform to report "the daemon closed the install stream without a
/// result" for every install that had to pull an image the node did not
/// have yet. Inside a runtime the send therefore degrades to `try_send`,
/// which drops a line only when the reader is already a full channel
/// behind — exactly what the reporter's best-effort contract allows.
fn send_progress_line<T>(tx: &tokio::sync::mpsc::Sender<T>, line: T) {
    if tokio::runtime::Handle::try_current().is_ok() {
        let _ = tx.try_send(line);
    } else {
        let _ = tx.blocking_send(line);
    }
}

/// Run an install/upgrade worker body, turning a panic into an ordinary
/// failed outcome.
///
/// The terminal event is the stream's contract: the caller waits for a
/// result or an error, and a panicking `spawn_blocking` task delivers
/// neither — it drops its sender and the stream simply ends. The daemon
/// survives such a panic either way (it never reaches the runtime's own
/// threads), so the only thing lost is the reason, which is exactly what
/// the caller needs.
fn catching_panics<T>(operation: &str, body: impl FnOnce() -> Result<T>) -> Result<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(outcome) => outcome,
        Err(panic) => {
            let reason = panic_message(panic);
            // The panic hook has already logged the payload and its
            // location; this records which operation died with it.
            warn!(operation, reason, "worker panicked");
            Err(anyhow::anyhow!("{operation} panicked: {reason}"))
        }
    }
}

/// The message carried by a caught panic (`panic!("…")`, `expect`, a failed
/// assertion), or a placeholder for a payload that is not a string.
fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = panic.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = panic.downcast_ref::<String>() {
        text.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// Context of bearer-token (TCP) calls: full visibility — the platform
/// performs its own per-user permission checks before reaching the daemon.
/// Per-user API tokens are a follow-up (see docs/api.md). The unix-socket
/// listener builds a real per-user context from SO_PEERCRED instead
/// (see [`uds`]).
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
            tokens: TokenStore::new(token),
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

    /// Map of container name -> (app id, app uuid) for every installed
    /// docker app the caller can see.
    ///
    /// Built from each app's own `AppMeta.runtime`, never by looking for an
    /// `asc-` prefix on the container name: that prefix is a naming
    /// convention the daemon happens to use, not a claim of ownership, and
    /// an operator is free to name a hand-made container the same way.
    fn container_owners(
        &self,
        ctx: &UserContext,
    ) -> Result<std::collections::HashMap<String, (String, Option<String>)>> {
        let mut owners = std::collections::HashMap::new();
        for app in self.manager.list(ctx)? {
            if let crate::daemon::apps::meta::Runtime::Docker { container, .. } = &app.meta.runtime
            {
                owners.insert(
                    container.clone(),
                    (app.meta.id.clone(), app.meta.uuid.clone()),
                );
            }
        }
        Ok(owners)
    }

    pub async fn status(self: &Arc<Self>, ctx: UserContext) -> Result<(usize, usize)> {
        self.blocking(move |s| {
            let apps = s.manager.list(&ctx)?;
            let running = apps
                .iter()
                .filter(|a| a.state == crate::daemon::apps::RuntimeState::Running)
                .count();
            Ok((running, apps.len()))
        })
        .await
    }

    pub async fn list_apps(self: &Arc<Self>, ctx: UserContext) -> Result<Vec<AppStatus>> {
        self.blocking(move |s| s.manager.list(&ctx)).await
    }

    pub async fn get_app(self: &Arc<Self>, ctx: UserContext, id: String) -> Result<AppStatus> {
        self.blocking(move |s| s.manager.status(&ctx, &id)).await
    }

    pub async fn app_disk(
        self: &Arc<Self>,
        ctx: UserContext,
        id: String,
    ) -> Result<(AppMeta, crate::daemon::apps::disk::DiskUsage)> {
        self.blocking(move |s| {
            let meta = s.manager.get_authorized(&ctx, &id)?;
            let usage = crate::daemon::apps::disk::usage(&s.config, s.manager.store(), &meta)?;
            Ok((meta, usage))
        })
        .await
    }

    /// Space taken by every app the caller may see, largest first, with the
    /// capacity of the filesystem holding the app store (DMN-053). The sizes
    /// are the cheap directory walk `asc stats` uses — no image or volume
    /// breakdown, no Docker queries.
    pub async fn disk_summary(self: &Arc<Self>, ctx: UserContext) -> Result<DiskSummary> {
        use crate::daemon::apps::disk;
        use crate::daemon::monitor::system;

        self.blocking(move |s| {
            let mut apps: Vec<AppDiskRow> = s
                .manager
                .list(&ctx)?
                .into_iter()
                .map(|app| AppDiskRow {
                    bytes: s
                        .manager
                        .store()
                        .app_dir(&app.meta.id)
                        .map(|dir| disk::dir_size(&dir))
                        .unwrap_or(0),
                    id: app.meta.id,
                    name: app.meta.custom_name.unwrap_or(app.meta.name),
                    owner: app.meta.owner.name,
                })
                .collect();
            apps.sort_by_key(|row| std::cmp::Reverse(row.bytes));
            Ok(DiskSummary {
                fs_total: system::filesystem_total(s.manager.store().root()),
                apps,
            })
        })
        .await
    }

    /// The ports one app publishes (DMN-049), resolved from its settings —
    /// so a stopped app reports what it will bind on the next start.
    pub async fn app_ports(
        self: &Arc<Self>,
        ctx: UserContext,
        id: String,
    ) -> Result<(AppMeta, Vec<crate::daemon::docker::PublishedPort>)> {
        self.blocking(move |s| {
            let meta = s.manager.get_authorized(&ctx, &id)?;
            let ports = crate::daemon::apps::ports::published(&s.config, s.manager.store(), &meta)?;
            Ok((meta, ports))
        })
        .await
    }

    /// The same, for every app the caller may see. An app whose manifest
    /// cannot be read reports no ports rather than failing the report.
    pub async fn ports_summary(self: &Arc<Self>, ctx: UserContext) -> Result<Vec<AppPortsRow>> {
        self.blocking(move |s| {
            Ok(s.manager
                .list(&ctx)?
                .into_iter()
                .map(|app| AppPortsRow {
                    ports: crate::daemon::apps::ports::published(
                        &s.config,
                        s.manager.store(),
                        &app.meta,
                    )
                    .unwrap_or_default(),
                    id: app.meta.id,
                    name: app.meta.custom_name.unwrap_or(app.meta.name),
                    owner: app.meta.owner.name,
                })
                .collect())
        })
        .await
    }

    /// Resource consumption of the caller's apps. Blocks for the sampling
    /// interval (~500 ms, two readings apart) on a worker thread.
    pub async fn stats(
        self: &Arc<Self>,
        ctx: UserContext,
    ) -> Result<Vec<crate::daemon::apps::AppStats>> {
        self.blocking(move |s| s.manager.stats(&ctx)).await
    }

    /// Resource consumption of `ids` only (DMN-080); empty means every app
    /// the caller can see. See `AppManager::stats_for` for why the filter
    /// happens before sampling.
    pub async fn stats_for(
        self: &Arc<Self>,
        ctx: UserContext,
        ids: Vec<String>,
    ) -> Result<Vec<crate::daemon::apps::AppStats>> {
        self.blocking(move |s| s.manager.stats_for(&ctx, &ids))
            .await
    }

    /// Upgrade an app the caller owns (DMN-053): `spec` is its id or custom
    /// name, optionally `@version`. Cloning happens with the daemon's own git
    /// credentials, like an install over this API.
    pub async fn upgrade(
        self: &Arc<Self>,
        ctx: UserContext,
        spec: String,
    ) -> Result<pkg::UpgradeOutcome> {
        self.blocking(move |s| pkg::upgrade(&s.config, &ctx, &spec, None))
            .await
    }

    /// Streamed sibling of [`Self::upgrade`]: progress lines arrive as
    /// [`UpgradeStreamEvent::Line`] as they happen, ending in one
    /// [`UpgradeStreamEvent::Done`] with the same result `upgrade` would
    /// have returned. Mirrors [`Self::install_stream`] exactly — the upgrade
    /// keeps running in the background regardless of whether the receiver is
    /// still being read.
    pub fn upgrade_stream(
        self: &Arc<Self>,
        ctx: UserContext,
        spec: String,
    ) -> tokio::sync::mpsc::Receiver<UpgradeStreamEvent> {
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        struct ChannelReporter(tokio::sync::mpsc::Sender<UpgradeStreamEvent>);
        impl progress::InstallReporter for ChannelReporter {
            fn line(&self, text: &str) {
                send_progress_line(&self.0, UpgradeStreamEvent::Line(text.to_string()));
            }
        }

        let state = Arc::clone(self);
        let result_tx = tx.clone();
        tokio::task::spawn_blocking(move || {
            let reporter = ChannelReporter(tx);
            let outcome = catching_panics("upgrade", || {
                pkg::upgrade(&state.config, &ctx, &spec, Some(&reporter))
            });
            // The terminal event, unlike a progress line, is never sent
            // from inside a runtime and must never be dropped: a full
            // channel means a slow reader, not a lost result.
            let _ = result_tx.blocking_send(UpgradeStreamEvent::Done(outcome));
        });
        rx
    }

    /// A repository's tags, newest first, and which one `InstallApp`/
    /// `UpgradeApp` would pick with no version given — one `git ls-remote`
    /// round trip, no clone (DMN-0XX). Feeds a version picker for both the
    /// install wizard (before the app exists) and the upgrade dropdown (an
    /// already-installed app, resolved to a git URL by the caller).
    pub async fn list_app_versions(
        self: &Arc<Self>,
        ctx: UserContext,
        git_url: String,
    ) -> Result<pkg::gitref::RemoteRefs> {
        self.blocking(move |_s| pkg::gitref::ls_remote(&git_url, &ctx))
            .await
    }

    /// What a package repository ships — one app or a stack of them, with
    /// the stack's apps and their declared requirements (DMN-098). A shallow
    /// clone into a temporary directory that is thrown away again: nothing is
    /// installed, no app directory is created. The install dialog calls it to
    /// show what an install is about to put on the node.
    pub async fn inspect_package(
        self: &Arc<Self>,
        ctx: UserContext,
        git_url: String,
        branch: Option<String>,
        tag: Option<String>,
        path: Option<String>,
    ) -> Result<pkg::PackageInfo> {
        self.blocking(move |_s| {
            let git_ref = match (branch.as_deref(), tag.as_deref()) {
                (Some(b), None) => Some(pkg::GitRef::Branch(b)),
                (None, Some(t)) => Some(pkg::GitRef::Tag(t)),
                (None, None) => None,
                (Some(_), Some(_)) => anyhow::bail!("pass either branch or tag, not both"),
            };
            pkg::inspect_git(&git_url, git_ref, path.as_deref(), &ctx)
        })
        .await
    }

    /// Install from a registry spec or directly from a git URL (mirrors the
    /// CLI's dispatch). Without `license_ack` a repository shipping a
    /// LICENSE returns the typed [`pkg::LicenseRequired`] error — REST
    /// serializes it structurally (`license_required`) so the CLI over the
    /// unix socket can render its own consent prompt and retry; the gRPC
    /// layer (`api::grpc::install_app`/`install_app_stream`, DMN-091) catches
    /// the same error and turns it into a normal `InstallAppResponse` with
    /// `license_required` set, so the platform UI gets the same fields
    /// without it ever reaching a gRPC error status. Without `force`, a host
    /// that cannot currently cover the package's requirements or runtime
    /// quota is caught the same way (DMN-099) — [`pkg::RequirementsNotMet`],
    /// surfaced as `requirements_not_met` rather than `license_required`.
    #[allow(clippy::too_many_arguments)]
    pub async fn install(
        self: &Arc<Self>,
        ctx: UserContext,
        spec: String,
        source: Option<String>,
        name: Option<String>,
        branch: Option<String>,
        tag: Option<String>,
        path: Option<String>,
        stack_app: Option<String>,
        license_ack: bool,
        image_choice: Option<crate::daemon::apps::ImageSource>,
        force: bool,
    ) -> Result<pkg::InstallOutcome> {
        self.blocking(move |s| {
            if pkg::is_git_url(&spec) {
                if source.is_some() {
                    anyhow::bail!("--source has no effect on a direct repository install");
                }
                let git_ref = match (branch.as_deref(), tag.as_deref()) {
                    (Some(b), None) => Some(pkg::GitRef::Branch(b)),
                    (None, Some(t)) => Some(pkg::GitRef::Tag(t)),
                    (None, None) => None,
                    (Some(_), Some(_)) => anyhow::bail!("pass either branch or tag, not both"),
                };
                return pkg::install_from_git(
                    &s.config,
                    &ctx,
                    &spec,
                    git_ref,
                    path.as_deref(),
                    stack_app.as_deref(),
                    name.as_deref(),
                    license_ack,
                    image_choice,
                    force,
                    None,
                );
            }
            if branch.is_some() || tag.is_some() || path.is_some() || stack_app.is_some() {
                anyhow::bail!(
                    "branch, tag, path and stack_app are only used for a direct repository install (a registry spec carries the version as '@version' and a stack app as '<stack>/<app>')"
                );
            }
            pkg::install(
                &s.config,
                &ctx,
                &spec,
                source.as_deref(),
                name.as_deref(),
                license_ack,
                image_choice,
                force,
                None,
            )
        })
        .await
    }

    /// Streamed sibling of [`Self::install`] (DMN-090): the same install,
    /// but progress lines arrive as [`InstallStreamEvent::Line`] as they
    /// happen, ending in one [`InstallStreamEvent::Done`] with the same
    /// result `install` would have returned. The install runs in the
    /// background regardless of whether the receiver is still being read —
    /// dropping it does not cancel the install, the same "finish what was
    /// started" stance `install` already takes for a caller that goes away
    /// mid-request.
    #[allow(clippy::too_many_arguments)]
    pub fn install_stream(
        self: &Arc<Self>,
        ctx: UserContext,
        spec: String,
        source: Option<String>,
        name: Option<String>,
        branch: Option<String>,
        tag: Option<String>,
        path: Option<String>,
        stack_app: Option<String>,
        license_ack: bool,
        image_choice: Option<crate::daemon::apps::ImageSource>,
        force: bool,
    ) -> tokio::sync::mpsc::Receiver<InstallStreamEvent> {
        // A line per subscriber's outstanding capacity: git/docker can emit
        // many lines quickly, and blocking the install itself on a slow
        // reader would defeat the point of streaming rather than waiting.
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        struct ChannelReporter(tokio::sync::mpsc::Sender<InstallStreamEvent>);
        impl progress::InstallReporter for ChannelReporter {
            fn line(&self, text: &str) {
                // Best effort: a full or closed channel (a caller that
                // stopped reading) must not slow down or panic the install
                // that is still running.
                send_progress_line(&self.0, InstallStreamEvent::Line(text.to_string()));
            }
        }

        let state = Arc::clone(self);
        let result_tx = tx.clone();
        tokio::task::spawn_blocking(move || {
            let reporter = ChannelReporter(tx);
            let outcome = catching_panics("install", move || {
                if pkg::is_git_url(&spec) {
                    if source.is_some() {
                        anyhow::bail!("--source has no effect on a direct repository install");
                    }
                    let git_ref = match (branch.as_deref(), tag.as_deref()) {
                        (Some(b), None) => Some(pkg::GitRef::Branch(b)),
                        (None, Some(t)) => Some(pkg::GitRef::Tag(t)),
                        (None, None) => None,
                        (Some(_), Some(_)) => anyhow::bail!("pass either branch or tag, not both"),
                    };
                    return pkg::install_from_git(
                        &state.config,
                        &ctx,
                        &spec,
                        git_ref,
                        path.as_deref(),
                        stack_app.as_deref(),
                        name.as_deref(),
                        license_ack,
                        image_choice,
                        force,
                        Some(&reporter),
                    );
                }
                if branch.is_some() || tag.is_some() || path.is_some() || stack_app.is_some() {
                    anyhow::bail!(
                        "branch, tag, path and stack_app are only used for a direct repository install (a registry spec carries the version as '@version' and a stack app as '<stack>/<app>')"
                    );
                }
                pkg::install(
                    &state.config,
                    &ctx,
                    &spec,
                    source.as_deref(),
                    name.as_deref(),
                    license_ack,
                    image_choice,
                    force,
                    Some(&reporter),
                )
            });
            // See upgrade_stream: the terminal event is always sent from
            // the blocking worker itself and always waits for the reader.
            let _ = result_tx.blocking_send(InstallStreamEvent::Done(outcome));
        });
        rx
    }

    pub async fn rename(
        self: &Arc<Self>,
        ctx: UserContext,
        id: String,
        name: String,
    ) -> Result<AppStatus> {
        self.blocking(move |s| s.manager.rename(&ctx, &id, &name))
            .await
    }

    pub async fn start(self: &Arc<Self>, ctx: UserContext, id: String) -> Result<Outcome> {
        self.blocking(move |s| s.manager.start(&ctx, &id)).await
    }

    pub async fn stop(self: &Arc<Self>, ctx: UserContext, id: String) -> Result<Outcome> {
        self.blocking(move |s| s.manager.stop(&ctx, &id)).await
    }

    pub async fn restart(self: &Arc<Self>, ctx: UserContext, id: String) -> Result<()> {
        self.blocking(move |s| s.manager.restart(&ctx, &id)).await
    }

    pub async fn logs(
        self: &Arc<Self>,
        ctx: UserContext,
        id: String,
        tail: usize,
        timestamps: bool,
    ) -> Result<String> {
        self.blocking(move |s| s.manager.logs(&ctx, &id, tail, timestamps))
            .await
    }

    pub async fn remove(self: &Arc<Self>, ctx: UserContext, id: String) -> Result<()> {
        self.blocking(move |s| s.manager.remove(&ctx, &id)).await
    }

    /// Create a backup through the daemon's local, peer-authenticated API.
    /// The local MCP transport deliberately uses only the built-in storage:
    /// user-specific storage credentials belong to the caller's home and are
    /// not available to a system daemon without weakening that boundary.
    pub async fn create_backup(
        self: &Arc<Self>,
        ctx: UserContext,
        id: String,
    ) -> Result<crate::daemon::backup::BackupInfo> {
        self.blocking(move |s| {
            use crate::daemon::backup::{self, storage};
            let meta = s.manager.get_authorized(&ctx, &id)?;
            let storages =
                storage::StorageList::load_with(crate::daemon::pkg::sources::Scope::System)?;
            backup::create_backup(
                &s.config,
                s.manager.store(),
                &meta,
                &storages,
                storage::LOCAL_NAME,
                None,
            )
        })
        .await
    }

    pub async fn list_backups(
        self: &Arc<Self>,
        ctx: UserContext,
        id: String,
    ) -> Result<Vec<String>> {
        self.blocking(move |s| {
            use crate::daemon::backup::{self, storage};
            let meta = s.manager.get_authorized(&ctx, &id)?;
            let storages =
                storage::StorageList::load_with(crate::daemon::pkg::sources::Scope::System)?;
            backup::list_backups(&s.config, &storages, storage::LOCAL_NAME, &meta.id)
        })
        .await
    }

    pub async fn restore_backup(
        self: &Arc<Self>,
        ctx: UserContext,
        id: String,
        backup_name: String,
    ) -> Result<()> {
        self.blocking(move |s| {
            use crate::daemon::backup::{self, storage};
            let status = s.manager.status(&ctx, &id)?;
            if status.state == crate::daemon::apps::RuntimeState::Running {
                anyhow::bail!(
                    "app '{}' must be stopped before restoring a backup",
                    status.meta.id
                );
            }
            let storages =
                storage::StorageList::load_with(crate::daemon::pkg::sources::Scope::System)?;
            backup::restore_backup(
                &s.config,
                s.manager.store(),
                &status.meta,
                &storages,
                storage::LOCAL_NAME,
                &backup_name,
            )
        })
        .await
    }

    pub async fn prune_backups(
        self: &Arc<Self>,
        ctx: UserContext,
        id: String,
        keep: u32,
    ) -> Result<Vec<String>> {
        self.blocking(move |s| {
            use crate::daemon::backup::{self, storage};
            let meta = s.manager.get_authorized(&ctx, &id)?;
            let storages =
                storage::StorageList::load_with(crate::daemon::pkg::sources::Scope::System)?;
            let store = backup::resolve_storage(&s.config, &storages, storage::LOCAL_NAME)?;
            backup::prune(store.as_ref(), &meta.id, keep)
        })
        .await
    }

    /// An app's settings schema and the values chosen so far (DMN-043): what
    /// an editor running outside the daemon — the CLI of a user who cannot
    /// read the system app tree — needs to render the same menu it renders
    /// in-process. `None` for an app whose package defines no settings.
    pub async fn app_settings(
        self: &Arc<Self>,
        ctx: UserContext,
        id: String,
    ) -> Result<(
        Option<pkg::settings::SettingsFile>,
        pkg::settings::SettingValues,
    )> {
        self.blocking(move |s| {
            let (file, mut values, _) = s.settings_of(&ctx, &id)?;
            if let Some(file) = &file {
                values.merge_defaults(&file.settings);
            }
            Ok((file, values))
        })
        .await
    }

    /// Replace an app's chosen values, validated against its own schema.
    /// The runtime picks them up on the next (re)start, exactly as it does
    /// after an in-process edit. Returns whether the app is currently
    /// running with a live configuration that would now drift from these
    /// values — the caller (DMN-078's `SetAppSettings`) surfaces this as
    /// "restart required"; a stopped app always answers `false`, since its
    /// values simply apply cleanly on the next start.
    pub async fn set_app_settings(
        self: &Arc<Self>,
        ctx: UserContext,
        id: String,
        values: pkg::settings::SettingValues,
    ) -> Result<bool> {
        self.blocking(move |s| {
            let (file, _, config_dir) = s.settings_of(&ctx, &id)?;
            let defs = file.as_ref().map(|f| f.settings.as_slice()).unwrap_or(&[]);
            values.validate_against(defs)?;
            std::fs::create_dir_all(&config_dir)
                .with_context(|| format!("cannot create directory {}", config_dir.display()))?;
            values.save(&config_dir)?;
            let status = s.manager.status(&ctx, &id)?;
            if status.state != RuntimeState::Running {
                return Ok(false);
            }
            let app_dir = s.manager.store().app_dir(&status.meta.id)?;
            crate::daemon::pkg::refresh::would_require_restart(&s.config, &status.meta, &app_dir)
        })
        .await
    }

    /// `(schema, current values, config dir)` of an app the caller may
    /// manage — the shared half of the two settings operations.
    fn settings_of(
        &self,
        ctx: &UserContext,
        id: &str,
    ) -> Result<(
        Option<pkg::settings::SettingsFile>,
        pkg::settings::SettingValues,
        std::path::PathBuf,
    )> {
        use pkg::settings::{SettingValues, SettingsFile, manifest_dir_of};
        let meta = self.manager.get_authorized(ctx, id)?;
        let app_dir = self.manager.store().app_dir(&meta.id)?;
        let manifest_dir = manifest_dir_of(&self.config, &app_dir)?;
        let manifest = pkg::manifest::Manifest::load(&manifest_dir)?;
        let file = SettingsFile::load_for(&manifest_dir, &manifest)?;
        let config_dir = app_dir.join("config");
        let values = SettingValues::load(&config_dir)?;
        Ok((file, values, config_dir))
    }

    /// Issue a one-time console token after verifying the app exists.
    pub async fn issue_console_token(
        self: &Arc<Self>,
        ctx: UserContext,
        app_id: String,
        session: console::SessionType,
        command: Vec<String>,
    ) -> Result<(String, i64)> {
        let id = app_id.clone();
        // Existence + authorization check first: no tokens for unknown apps.
        self.blocking(move |s| s.manager.get_authorized(&ctx, &id))
            .await?;
        Ok(self.console_tokens.issue(&app_id, session, command))
    }

    // ── Registry sources & credentials (DMN-083/084/087) — pushed by the
    // platform, see docs/custom-registry.md and docs/package-manager.md.
    // Every call below acts on Scope::System: the daemon process itself is
    // always root, so unlike AppManager there is no per-caller-uid
    // branching to do here.

    pub async fn list_sources(self: &Arc<Self>) -> Result<Vec<pkg::sources::Source>> {
        self.blocking(|_s| {
            let list = pkg::sources::SourceList::load_with(pkg::sources::Scope::System)?;
            Ok(list.list().into_iter().map(|(s, _)| s.clone()).collect())
        })
        .await
    }

    pub async fn replace_sources(
        self: &Arc<Self>,
        sources: Vec<pkg::sources::Source>,
    ) -> Result<Vec<pkg::sources::Source>> {
        self.blocking(move |_s| {
            let mut list = pkg::sources::SourceList::load_with(pkg::sources::Scope::System)?;
            list.replace_all(sources)?;
            list.save()?;
            Ok(list.list().into_iter().map(|(s, _)| s.clone()).collect())
        })
        .await
    }

    pub async fn list_credentials(self: &Arc<Self>) -> Result<Vec<pkg::auth::Credential>> {
        self.blocking(|_s| {
            let auth = pkg::auth::GitAuth::load_with(pkg::sources::Scope::System)?;
            Ok(auth.list().into_iter().map(|(c, _)| c.clone()).collect())
        })
        .await
    }

    pub async fn upsert_credential(
        self: &Arc<Self>,
        kind: pkg::auth::Kind,
        target: String,
        secret: pkg::auth::CredentialSecret,
        username: Option<String>,
        app: Option<String>,
    ) -> Result<pkg::auth::Credential> {
        self.blocking(move |_s| {
            let mut auth = pkg::auth::GitAuth::load_with(pkg::sources::Scope::System)?;
            let credential = match secret {
                pkg::auth::CredentialSecret::Token(token) => auth
                    .add(
                        kind,
                        &target,
                        pkg::auth::Method::Token { token },
                        username,
                        app,
                    )?
                    .clone(),
                pkg::auth::CredentialSecret::SshKeyPem(pem) => auth
                    .add_ssh_key(kind, &target, &pem, username, app)?
                    .clone(),
            };
            auth.save()?;
            Ok(credential)
        })
        .await
    }

    pub async fn remove_credential(
        self: &Arc<Self>,
        kind: Option<pkg::auth::Kind>,
        target: String,
    ) -> Result<()> {
        self.blocking(move |_s| {
            let mut auth = pkg::auth::GitAuth::load_with(pkg::sources::Scope::System)?;
            auth.remove(kind, &target)?;
            auth.save()
        })
        .await
    }

    /// Request a whole-host reboot after the API response has left the
    /// machine. The fixed `systemctl` invocation deliberately exposes no
    /// caller-supplied command surface.
    pub async fn reboot_system(self: &Arc<Self>) -> Result<()> {
        let _ = self;
        tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            if cfg!(test) {
                return;
            }
            match tokio::process::Command::new("systemctl")
                .args(["reboot", "--no-wall"])
                .status()
                .await
            {
                Ok(status) if status.success() => {}
                Ok(status) => warn!(%status, "system reboot request was rejected"),
                Err(error) => warn!(%error, "could not start system reboot request"),
            }
        });
        Ok(())
    }

    // ── Files (DMN-070): node filesystem access from "/", see docs/files.md.
    // Every entry point starts with `files::require_root` — the unix socket
    // is otherwise world-connectable and authorizes purely by peer uid, a
    // rule this service must not inherit. The TCP transport (platform) is
    // unaffected: `api_context()` above already carries `is_root: true`.
    //
    // With `app_id` set, a call is app-scoped instead (DMN-086): the caller
    // only needs to own the app (`AppManager::get_authorized`, the same
    // ownership check every other per-app method uses — root included), and
    // every path is confined to that app's directory and private volumes by
    // `files::AppScope`, resolved and enforced daemon-side regardless of
    // what rights the transport itself carries. That confinement is the
    // whole point: the TCP transport is always full-rights, so a platform
    // user with `apps.edit` but not `files.edit` must still be unable to
    // reach anything outside their own app through this path. See
    // `asc-platform/docs/features/app-file-manager.md`.

    /// Root confinement for one call: `None` (unscoped) requires a root
    /// context, same as always; `Some(app_id)` requires only that the
    /// caller owns that app and confines every path the call touches to it.
    fn app_file_scope(
        &self,
        ctx: &UserContext,
        app_id: &Option<String>,
    ) -> Result<Option<files::AppScope>> {
        match app_id {
            Some(id) => {
                let meta = self.manager.get_authorized(ctx, id)?;
                Ok(Some(files::AppScope::for_app(
                    &self.config,
                    self.manager.store(),
                    &meta,
                )?))
            }
            None => {
                files::require_root(ctx)?;
                Ok(None)
            }
        }
    }

    pub async fn list_directory(
        self: &Arc<Self>,
        ctx: UserContext,
        app_id: Option<String>,
        path: String,
        include_hidden: bool,
    ) -> Result<files::Listing> {
        self.blocking(move |s| {
            let scope = s.app_file_scope(&ctx, &app_id)?;
            Ok(files::list_directory(
                &path,
                include_hidden,
                scope.as_ref(),
            )?)
        })
        .await
    }

    pub async fn stat_path(
        self: &Arc<Self>,
        ctx: UserContext,
        app_id: Option<String>,
        path: String,
    ) -> Result<(files::FileEntry, String)> {
        self.blocking(move |s| {
            let scope = s.app_file_scope(&ctx, &app_id)?;
            Ok(files::stat(&path, scope.as_ref())?)
        })
        .await
    }

    pub async fn create_directory(
        self: &Arc<Self>,
        ctx: UserContext,
        app_id: Option<String>,
        path: String,
        parents: bool,
    ) -> Result<files::FileEntry> {
        self.blocking(move |s| {
            let scope = s.app_file_scope(&ctx, &app_id)?;
            Ok(files::create_directory(&path, parents, scope.as_ref())?)
        })
        .await
    }

    pub async fn move_path(
        self: &Arc<Self>,
        ctx: UserContext,
        app_id: Option<String>,
        source: String,
        destination: String,
        overwrite: bool,
    ) -> Result<files::FileEntry> {
        self.blocking(move |s| {
            let scope = s.app_file_scope(&ctx, &app_id)?;
            Ok(files::move_path(
                &source,
                &destination,
                overwrite,
                scope.as_ref(),
            )?)
        })
        .await
    }

    pub async fn copy_path(
        self: &Arc<Self>,
        ctx: UserContext,
        app_id: Option<String>,
        source: String,
        destination: String,
        overwrite: bool,
    ) -> Result<(files::FileEntry, u64, u32)> {
        self.blocking(move |s| {
            let scope = s.app_file_scope(&ctx, &app_id)?;
            Ok(files::copy_path(
                &source,
                &destination,
                overwrite,
                scope.as_ref(),
            )?)
        })
        .await
    }

    pub async fn delete_paths(
        self: &Arc<Self>,
        ctx: UserContext,
        app_id: Option<String>,
        paths: Vec<String>,
        recursive: bool,
    ) -> Result<(u32, Vec<(String, String)>)> {
        self.blocking(move |s| {
            let scope = s.app_file_scope(&ctx, &app_id)?;
            Ok(files::delete_paths(&paths, recursive, scope.as_ref()))
        })
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_archive(
        self: &Arc<Self>,
        ctx: UserContext,
        app_id: Option<String>,
        directory: String,
        names: Vec<String>,
        archive_path: String,
        format: files::ArchiveFormat,
    ) -> Result<(files::FileEntry, u64, u32)> {
        self.blocking(move |s| {
            let scope = s.app_file_scope(&ctx, &app_id)?;
            Ok(files::create_archive(
                &directory,
                &names,
                &archive_path,
                format,
                scope.as_ref(),
            )?)
        })
        .await
    }

    /// Open a file for streaming download on a worker thread. Byte transfer
    /// does not fit [`Self::blocking`] (which hands back a single result):
    /// the read loop runs on its own `spawn_blocking` task, feeding chunks
    /// through a 4-deep bounded channel — back-pressure is the channel
    /// depth, and the async runtime never blocks on a disk read.
    pub async fn open_file_read(
        self: &Arc<Self>,
        ctx: UserContext,
        app_id: Option<String>,
        path: String,
        offset: u64,
    ) -> Result<(u64, tokio::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>)> {
        let scope = self
            .blocking(move |s| s.app_file_scope(&ctx, &app_id))
            .await?;
        let mut handle = tokio::task::spawn_blocking(move || {
            files::ReadHandle::open(&path, offset, scope.as_ref())
        })
        .await
        .context("file read worker panicked")??;
        let size = handle.size;
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; files::CHUNK_BYTES];
            loop {
                match handle.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.blocking_send(Ok(buf[..n].to_vec())).is_err() {
                            break; // the caller went away; stop reading
                        }
                    }
                    Err(err) => {
                        let _ = tx.blocking_send(Err(err));
                        break;
                    }
                }
            }
        });
        Ok((size, rx))
    }

    pub async fn set_file_attributes(
        self: &Arc<Self>,
        ctx: UserContext,
        app_id: Option<String>,
        path: String,
        mode: Option<u32>,
        owner: Option<String>,
        group: Option<String>,
    ) -> Result<files::FileEntry> {
        self.blocking(move |s| {
            let scope = s.app_file_scope(&ctx, &app_id)?;
            Ok(files::set_attributes(
                &path,
                mode,
                owner.as_deref(),
                group.as_deref(),
                scope.as_ref(),
            )?)
        })
        .await
    }

    pub async fn list_system_identities(
        self: &Arc<Self>,
        ctx: UserContext,
    ) -> Result<(Vec<files::SystemUser>, Vec<files::SystemGroup>)> {
        self.blocking(move |_| {
            files::require_root(&ctx)?;
            Ok(files::list_system_identities()?)
        })
        .await
    }

    // ── Docker host inventory (DMN-102/DMN-112, see docs/app-management.md) ──

    /// Every container on the node's Docker Engine, with the ones belonging
    /// to installed ASC apps identified.
    ///
    /// Root-only: a container ASC did not create has no owner for
    /// [`AppManager::get_authorized`] to check against, so there is nothing
    /// to scope a non-root caller to. TCP callers always present
    /// [`api_context`], so in practice this only bites the unix socket.
    pub async fn list_containers(
        self: &Arc<Self>,
        ctx: UserContext,
        all: bool,
        with_size: bool,
    ) -> Result<Vec<ContainerRow>> {
        self.blocking(move |s| {
            users::require_root(&ctx)?;
            let containers = docker::list_containers(&s.config.docker, all, with_size)?;
            let owners = s.container_owners(&ctx)?;
            Ok(containers
                .into_iter()
                .map(|info| {
                    let owner = info
                        .names
                        .iter()
                        .find_map(|name| owners.get(name.as_str()))
                        .cloned();
                    ContainerRow {
                        app_id: owner.as_ref().map(|(id, _)| id.clone()),
                        app_uuid: owner.and_then(|(_, uuid)| uuid),
                        info,
                    }
                })
                .collect())
        })
        .await
    }

    /// Live resource usage of `ids` (empty = every running container), like
    /// `docker stats --no-stream`.
    ///
    /// Two readings around one shared sleep, exactly as
    /// [`AppManager::stats_for`] does it: every container's first counter is
    /// taken, the thread sleeps once, then every second counter is taken. The
    /// sampling window is therefore ~500 ms in total rather than 500 ms per
    /// container, and the filter is applied *before* it — asking about one
    /// container must not cost the sampling time of all of them.
    pub async fn list_container_stats(
        self: &Arc<Self>,
        ctx: UserContext,
        ids: Vec<String>,
    ) -> Result<Vec<ContainerStatsRow>> {
        self.blocking(move |s| {
            users::require_root(&ctx)?;
            let cfg = &s.config.docker;
            // Resolve the id set from the live container list so that a name
            // works as well as an id, and so that "empty means all running"
            // needs no special case further down.
            let targets: Vec<String> = if ids.is_empty() {
                docker::list_containers(cfg, false, false)?
                    .into_iter()
                    .map(|info| info.id)
                    .collect()
            } else {
                ids
            };

            let first: Vec<Option<docker::ContainerUsage>> =
                targets.iter().map(|id| usage_or_warn(cfg, id)).collect();
            let started = std::time::Instant::now();
            std::thread::sleep(std::time::Duration::from_millis(STATS_SAMPLE_MILLIS));
            let elapsed_micros = started.elapsed().as_micros() as u64;

            let mut rows = Vec::with_capacity(targets.len());
            for (id, first) in targets.into_iter().zip(first) {
                let (Some(first), Some(second)) = (first, usage_or_warn(cfg, &id)) else {
                    // The container stopped or was removed between the two
                    // readings — an ordinary race, not a failed call. Leave
                    // it out rather than reporting zeroes.
                    continue;
                };
                rows.push(ContainerStatsRow {
                    id,
                    cpu_percent: cpu_percent_between(&first, &second, elapsed_micros),
                    memory_bytes: second.memory_bytes,
                    memory_limit_bytes: second.memory_limit_bytes,
                    net_rx_bytes: second.net_rx_bytes,
                    net_tx_bytes: second.net_tx_bytes,
                    block_read_bytes: second.disk_read_bytes,
                    block_write_bytes: second.disk_write_bytes,
                });
            }
            Ok(rows)
        })
        .await
    }

    /// Every image on the host, ASC-owned or not (DMN-104), with the
    /// protected ones (still run by an installed app) marked.
    pub async fn list_images(self: &Arc<Self>, ctx: UserContext) -> Result<Vec<DockerImageRow>> {
        self.blocking(move |s| {
            users::require_root(&ctx)?;
            let images = docker::list_images(&s.config.docker)?;
            let protection = DockerProtection::scan(s, &ctx)?;
            Ok(images
                .into_iter()
                .map(|image| {
                    let row = DockerImageRow {
                        id: image.id,
                        tags: image.tags,
                        size_bytes: image.size,
                        created: image.created,
                        labels: image.labels,
                        dangling: image.dangling,
                        asc_protected: false,
                        protected_reason: None,
                    };
                    let reason = protection
                        .image_reason(&row.tags, &row.id)
                        .map(str::to_string);
                    DockerImageRow {
                        asc_protected: reason.is_some(),
                        protected_reason: reason,
                        ..row
                    }
                })
                .collect())
        })
        .await
    }

    /// Every named volume on the host (DMN-104), with the protected ones
    /// (still declared by an installed app's settings) marked.
    pub async fn list_volumes(self: &Arc<Self>, ctx: UserContext) -> Result<Vec<DockerVolumeRow>> {
        self.blocking(move |s| {
            users::require_root(&ctx)?;
            let volumes = docker::list_volumes(&s.config.docker)?;
            let protection = DockerProtection::scan(s, &ctx)?;
            Ok(volumes
                .into_iter()
                .map(|volume| {
                    let reason = protection.volume_reason(&volume.name).map(str::to_string);
                    DockerVolumeRow {
                        asc_protected: reason.is_some(),
                        protected_reason: reason,
                        name: volume.name,
                        driver: volume.driver,
                        mountpoint: volume.mountpoint,
                        created_at: volume.created_at,
                        labels: volume.labels,
                        ref_count: volume.ref_count.and_then(|c| i32::try_from(c).ok()),
                        size_bytes: volume.size_bytes,
                    }
                })
                .collect())
        })
        .await
    }

    /// Every network on the host (DMN-104), inventory-only — see
    /// [`Self::prune_docker`] for why networks are never a prune target.
    pub async fn list_networks(
        self: &Arc<Self>,
        ctx: UserContext,
    ) -> Result<Vec<DockerNetworkRow>> {
        self.blocking(move |s| {
            users::require_root(&ctx)?;
            Ok(docker::list_networks(&s.config.docker)?
                .into_iter()
                .map(|network| DockerNetworkRow {
                    id: network.id,
                    name: network.name,
                    driver: network.driver,
                    scope: network.scope,
                    internal: network.internal,
                    created: network.created,
                    labels: network.labels,
                })
                .collect())
        })
        .await
    }

    /// `docker system df`'s four categories (DMN-104). Expensive — the
    /// Engine walks every layer and volume to answer — so callers ask for it
    /// only when a user opens the Docker settings section, never on a poll.
    pub async fn docker_disk_usage(
        self: &Arc<Self>,
        ctx: UserContext,
    ) -> Result<DockerDiskUsageRow> {
        self.blocking(move |s| {
            users::require_root(&ctx)?;
            let usage = docker::disk_usage(&s.config.docker)?;
            Ok(DockerDiskUsageRow {
                images: usage.images.into(),
                containers: usage.containers.into(),
                volumes: usage.volumes.into(),
                build_cache: usage.build_cache.into(),
            })
        })
        .await
    }

    /// Remove unused images/volumes/build cache, one item at a time rather
    /// than the Engine's own bulk prune endpoints (DMN-105): an item an
    /// installed app still needs — running or stopped — is protected and
    /// reported in `skipped` with why, never silently removed. `dry_run`
    /// computes the exact same plan without deleting anything.
    ///
    /// Networks are deliberately not a target here: unlike images and
    /// volumes there is no ASC ownership signal to check a network against,
    /// and the Engine's own network prune removes any network with no
    /// *running* container attached — including a stopped compose stack's
    /// network, which would silently break its next `up`.
    pub async fn prune_docker(
        self: &Arc<Self>,
        ctx: UserContext,
        target: PruneTarget,
        dry_run: bool,
        dangling_only: bool,
    ) -> Result<PruneDockerRow> {
        self.blocking(move |s| {
            users::require_root(&ctx)?;
            match target {
                PruneTarget::Images => prune_images(s, &ctx, dry_run, dangling_only),
                PruneTarget::Volumes => prune_volumes(s, &ctx, dry_run),
                PruneTarget::BuildCache => prune_build_cache_target(s, dry_run),
            }
        })
        .await
    }

    /// Every real listening port on the host, merged with the two things
    /// `/proc` cannot say (DMN-103): which Docker container owns a
    /// `docker-proxy`/`dockerd` listener, and which ports a stopped app
    /// would bind on its next start.
    ///
    /// The Docker cross-reference is non-fatal: an unreachable Engine
    /// leaves every socket unattributed rather than failing the call — the
    /// `/proc` inventory is still useful on its own.
    pub async fn listening_ports(
        self: &Arc<Self>,
        ctx: UserContext,
    ) -> Result<Vec<ListeningPortRow>> {
        self.blocking(move |s| {
            let sockets = crate::daemon::monitor::sockets::listening();

            let containers = match docker::list_containers(&s.config.docker, false, false) {
                Ok(containers) => containers,
                Err(err) => {
                    warn!(error = %format!("{err:#}"), "cannot query docker for port attribution");
                    Vec::new()
                }
            };
            let owners = s.container_owners(&ctx)?;
            let container_ports = ContainerPortIndex::build(&containers, &owners);

            let daemon_ports: Vec<u16> = [
                listen_port(&s.config.api.listen),
                s.config
                    .api
                    .acme_http_listen
                    .as_deref()
                    .and_then(listen_port),
            ]
            .into_iter()
            .flatten()
            .collect();

            // (port, transport) already covered by a live socket — the
            // declared-ports pass below must not duplicate these.
            let mut live: std::collections::HashSet<(u16, &'static str)> =
                std::collections::HashSet::new();

            let mut rows: Vec<ListeningPortRow> = sockets
                .into_iter()
                .map(|socket| {
                    live.insert((socket.port, socket.protocol));
                    let attributed = container_ports.lookup(socket.port, socket.protocol);
                    let (container_id, container_name, app_id, app_uuid) = match attributed {
                        Some((id, name, app_id, app_uuid)) => {
                            (Some(id.clone()), Some(name.clone()), app_id.clone(), app_uuid.clone())
                        }
                        None => (None, None, None, None),
                    };
                    ListeningPortRow {
                        is_daemon: socket.protocol == "tcp" && daemon_ports.contains(&socket.port),
                        port: socket.port,
                        protocol: socket.protocol,
                        address: socket.address,
                        family: socket.family,
                        pid: socket.pid,
                        process: socket.process,
                        command: socket.command,
                        container_id,
                        container_name,
                        app_id,
                        app_uuid,
                        declared_only: false,
                    }
                })
                .collect();

            // Declared-but-unbound ports of stopped docker apps: a reserved
            // port must still show up, or it silently looks free.
            for app in s.manager.list(&ctx)? {
                if !matches!(
                    app.meta.runtime,
                    crate::daemon::apps::meta::Runtime::Docker { .. }
                ) {
                    continue;
                }
                let declared = match crate::daemon::apps::ports::published(
                    &s.config,
                    s.manager.store(),
                    &app.meta,
                ) {
                    Ok(ports) => ports,
                    Err(err) => {
                        warn!(app = %app.meta.id, error = %format!("{err:#}"), "cannot resolve declared ports");
                        continue;
                    }
                };
                for declared_port in declared {
                    for transport in declared_port.protocol.transports() {
                        if live.contains(&(declared_port.host, *transport)) {
                            continue;
                        }
                        rows.push(ListeningPortRow {
                            port: declared_port.host,
                            protocol: transport,
                            address: String::new(),
                            family: "",
                            pid: None,
                            process: None,
                            command: None,
                            container_id: None,
                            container_name: None,
                            app_id: Some(app.meta.id.clone()),
                            app_uuid: app.meta.uuid.clone(),
                            declared_only: true,
                            is_daemon: false,
                        });
                    }
                }
            }

            Ok(rows)
        })
        .await
    }

    // ── Local account management (DMN-100, see docs/user-management.md) ──

    pub async fn list_users(self: &Arc<Self>, ctx: UserContext) -> Result<Vec<users::ManagedUser>> {
        self.blocking(move |_| {
            users::require_root(&ctx)?;
            Ok(users::list_users()?)
        })
        .await
    }

    pub async fn create_user(
        self: &Arc<Self>,
        ctx: UserContext,
        name: String,
        home: Option<String>,
        shell: Option<String>,
        create_home: Option<bool>,
        groups: Vec<String>,
    ) -> Result<users::ManagedUser> {
        self.blocking(move |_| {
            users::require_root(&ctx)?;
            Ok(users::create_user(
                &name,
                home.as_deref(),
                shell.as_deref(),
                create_home,
                &groups,
            )?)
        })
        .await
    }

    pub async fn delete_user(
        self: &Arc<Self>,
        ctx: UserContext,
        name: String,
        remove_home: bool,
    ) -> Result<()> {
        self.blocking(move |_| {
            users::require_root(&ctx)?;
            Ok(users::delete_user(&name, remove_home)?)
        })
        .await
    }

    pub async fn set_user_locked(
        self: &Arc<Self>,
        ctx: UserContext,
        name: String,
        locked: bool,
    ) -> Result<users::ManagedUser> {
        self.blocking(move |_| {
            users::require_root(&ctx)?;
            Ok(users::set_user_locked(&name, locked)?)
        })
        .await
    }

    pub async fn set_user_shell(
        self: &Arc<Self>,
        ctx: UserContext,
        name: String,
        shell: String,
    ) -> Result<users::ManagedUser> {
        self.blocking(move |_| {
            users::require_root(&ctx)?;
            Ok(users::set_user_shell(&name, &shell)?)
        })
        .await
    }

    pub async fn set_user_groups(
        self: &Arc<Self>,
        ctx: UserContext,
        name: String,
        groups: Vec<String>,
    ) -> Result<users::ManagedUser> {
        self.blocking(move |_| {
            users::require_root(&ctx)?;
            Ok(users::set_user_groups(&name, &groups)?)
        })
        .await
    }

    pub async fn list_authorized_keys(
        self: &Arc<Self>,
        ctx: UserContext,
        user: String,
    ) -> Result<Vec<users::AuthorizedKey>> {
        self.blocking(move |_| {
            users::require_root(&ctx)?;
            Ok(users::list_authorized_keys(&user)?)
        })
        .await
    }

    pub async fn add_authorized_key(
        self: &Arc<Self>,
        ctx: UserContext,
        user: String,
        public_key: String,
    ) -> Result<users::AuthorizedKey> {
        self.blocking(move |_| {
            users::require_root(&ctx)?;
            Ok(users::add_authorized_key(&user, &public_key)?)
        })
        .await
    }

    pub async fn remove_authorized_key(
        self: &Arc<Self>,
        ctx: UserContext,
        user: String,
        fingerprint: String,
    ) -> Result<()> {
        self.blocking(move |_| {
            users::require_root(&ctx)?;
            Ok(users::remove_authorized_key(&user, &fingerprint)?)
        })
        .await
    }

    /// Open an upload sink on a worker thread; the mirror of
    /// [`Self::open_file_read`]. The returned sender feeds chunks in; the
    /// join handle resolves once the header, every chunk and the final
    /// atomic commit have all landed, or reports why they did not.
    pub async fn open_file_write(
        self: &Arc<Self>,
        ctx: UserContext,
        app_id: Option<String>,
        header: files::WriteHeader,
    ) -> Result<(
        tokio::sync::mpsc::Sender<Vec<u8>>,
        tokio::task::JoinHandle<Result<files::FileEntry>>,
    )> {
        let scope = self
            .blocking(move |s| s.app_file_scope(&ctx, &app_id))
            .await?;
        let mut handle =
            tokio::task::spawn_blocking(move || files::WriteHandle::open(&header, scope.as_ref()))
                .await
                .context("file write worker panicked")??;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
        let join = tokio::task::spawn_blocking(move || -> Result<files::FileEntry> {
            while let Some(chunk) = rx.blocking_recv() {
                handle.write_all(&chunk)?;
            }
            Ok(handle.commit()?)
        });
        Ok((tx, join))
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

/// Write the token file with root-only permissions, atomically.
///
/// A plain truncate-then-write here is a lockout waiting to happen: a crash
/// or a full disk in the middle of it leaves an empty `api.token`, and the
/// platform can never authenticate again. So the token goes to a temporary
/// file beside the target — created 0600 *before* it holds anything — is
/// fsynced, and only then renamed over the target. The directory is fsynced
/// too, so the new name survives a power loss.
pub fn write_token(path: &std::path::Path, token: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let dir = path.parent().filter(|d| !d.as_os_str().is_empty());
    if let Some(dir) = dir {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("cannot create directory {}", dir.display()))?;
    }

    let temp = path.with_extension("token.tmp");
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)
            .with_context(|| format!("cannot create token file {}", temp.display()))?;
        file.write_all(token.as_bytes())
            .with_context(|| format!("cannot write token file {}", temp.display()))?;
        file.sync_all()
            .with_context(|| format!("cannot flush token file {}", temp.display()))?;
    }
    std::fs::rename(&temp, path)
        .with_context(|| format!("cannot install token file {}", path.display()))?;
    if let Some(dir) = dir {
        // Best-effort: a filesystem that refuses to open a directory still
        // has the renamed file, it is only the crash guarantee that is lost.
        if let Ok(handle) = std::fs::File::open(dir) {
            let _ = handle.sync_all();
        }
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
    // Refuse a [api] section that cannot work before the port is bound: a
    // listener that comes up and then fails every handshake is worse than one
    // that never came up with a clear reason.
    state.config.api.validate()?;
    let listen = state.config.api.listen.clone();
    let materials = tls::prepare(&state.config)?;
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("cannot bind API listener on {listen}"))?;

    let Some(materials) = materials else {
        if !listen.starts_with("127.") && !listen.starts_with("localhost") {
            warn!(
                addr = %listen,
                "the API listens beyond loopback without TLS; the bearer token                  travels unencrypted. Set [api] tls = \"self_signed\""
            );
        }
        info!(addr = %listen, "API listening (gRPC + REST)");
        return axum::serve(listener, router(state))
            .with_graceful_shutdown(shutdown)
            .await
            .context("API server failed");
    };

    info!(
        addr = %listen,
        fingerprint = %materials.fingerprint,
        "API listening over TLS (gRPC + REST)"
    );
    serve_tls(listener, router(state), materials, shutdown).await
}

/// TLS accept loop. axum::serve has no TLS support, and the API has to keep
/// speaking both protocols: h2 for gRPC, HTTP/1.1 for REST and the console
/// WebSocket. hyper's auto builder picks per connection from the ALPN result.
async fn serve_tls(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    materials: tls::Materials,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let acceptor = tokio_rustls::TlsAcceptor::from(materials.config);
    let mut shutdown = std::pin::pin!(shutdown);

    loop {
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => accepted.context("cannot accept a connection")?,
            () = &mut shutdown => return Ok(()),
        };
        let acceptor = acceptor.clone();
        let service = hyper_util::service::TowerToHyperService::new(app.clone());
        tokio::spawn(async move {
            let stream = match acceptor.accept(stream).await {
                Ok(stream) => stream,
                // A failed handshake is routine on a public port: scanners,
                // health checks and clients that reject the certificate.
                Err(err) => {
                    debug!(%peer, error = %err, "TLS handshake failed");
                    return;
                }
            };
            let io = hyper_util::rt::TokioIo::new(stream);
            if let Err(err) =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection_with_upgrades(io, service)
                    .await
            {
                debug!(%peer, error = %err, "connection ended");
            }
        });
    }
}

/// Bearer-token check for both transports. gRPC callers get a proper
/// `grpc-status: UNAUTHENTICATED` trailer-only response, REST callers 401.
/// Authenticated requests carry the full-visibility [`api_context`] — the
/// per-user context is the unix-socket listener's job (see [`uds`]).
///
/// Both token kinds authenticate the same way and get the same context; what
/// separates them is the [`tokens::require_primary`] guard on the handful of
/// token-management routes (DMN-065). The classification travels alongside
/// the context so those handlers can consult it.
async fn auth(state: Arc<ApiState>, mut req: Request<Body>, next: Next) -> Response {
    let resolved = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .and_then(|token| state.tokens.resolve(token));
    if let Some(resolved) = resolved {
        req.extensions_mut().insert(api_context());
        req.extensions_mut().insert(resolved);
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

    /// A progress line reported from inside an async context must reach the
    /// stream instead of killing it: Docker pulls and builds report from
    /// within `docker::block_on`'s current-thread runtime, entered on the
    /// blocking worker's own thread, where `blocking_send` panics.
    #[test]
    fn a_progress_line_reported_from_inside_a_runtime_reaches_the_stream() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<InstallStreamEvent>(4);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            send_progress_line(&tx, InstallStreamEvent::Line("Pulling image".into()));
        });
        drop(runtime);
        // And the plain blocking path (a git clone's own lines) still works.
        send_progress_line(&tx, InstallStreamEvent::Line("$ git clone".into()));
        drop(tx);

        let mut lines = Vec::new();
        while let Some(InstallStreamEvent::Line(line)) = rx.blocking_recv() {
            lines.push(line);
        }
        assert_eq!(lines, ["Pulling image", "$ git clone"]);
    }

    /// A worker that panics still ends its stream with a terminal event —
    /// a failed one naming the reason, not a stream that just stops.
    #[test]
    fn a_panicking_worker_ends_the_stream_with_an_error() {
        let outcome: Result<()> = catching_panics("install", || panic!("blocking_send exploded"));
        let message = outcome.unwrap_err().to_string();
        assert!(message.contains("install panicked"), "{message}");
        assert!(message.contains("blocking_send exploded"), "{message}");

        // A non-panicking body is passed through untouched.
        assert_eq!(catching_panics("install", || Ok(7)).unwrap(), 7);
    }

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

    /// The token file is replaced, never truncated in place: an interrupted
    /// write must not be able to leave an empty `api.token` behind and lock
    /// the platform out for good (DMN-066).
    #[test]
    fn the_token_file_is_replaced_atomically() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api.token");
        write_token(&path, "first").unwrap();
        write_token(&path, "second-and-longer").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second-and-longer");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // Nothing readable is left behind for someone to scoop up.
        assert!(!dir.path().join("api.token.tmp").exists());
    }

    /// The whole point of the split (DMN-065): an access token drives the
    /// daemon like the primary, right up to the routes that manage tokens.
    #[tokio::test]
    async fn an_access_token_may_work_but_may_not_manage_tokens() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.daemon.apps_dir = dir.path().join("apps");
        config.daemon.data_dir = dir.path().join("data");
        let state = ApiState::new(config, "primary-token".into());
        let (access, _) = state.tokens.issue_access(None, "test");

        let call = async |token: &str, method: &str, uri: &str| {
            router(Arc::clone(&state))
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .header(header::AUTHORIZATION, format!("Bearer {token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
                .status()
        };

        // An ordinary route: both kinds of token get through.
        assert_eq!(call(&access, "GET", "/v1/apps").await, StatusCode::OK);
        assert_eq!(
            call("primary-token", "GET", "/v1/apps").await,
            StatusCode::OK
        );
        // Token management: primary only. 403, not 401 — the credential is
        // valid, the operation is not open to it.
        assert_eq!(
            call(&access, "POST", "/v1/token/rotate").await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(&access, "DELETE", "/v1/token/access").await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(&access, "POST", "/v1/token/access").await,
            StatusCode::FORBIDDEN
        );
        // Status is open to everyone, and never carries the primary.
        assert_eq!(call(&access, "GET", "/v1/token").await, StatusCode::OK);
        // An unknown token still gets nowhere.
        assert_eq!(
            call("nonsense", "GET", "/v1/apps").await,
            StatusCode::UNAUTHORIZED
        );
    }
}
