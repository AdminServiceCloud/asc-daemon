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
use crate::daemon::apps::{AppManager, AppStatus, Outcome, UserContext};
use crate::daemon::config::Config;
use crate::daemon::files;
use crate::daemon::monitor::Monitor;
use crate::daemon::pkg;

use console::ConsoleTokens;
use tokens::TokenStore;

/// Optional features this daemon build supports, reported by `GetStatus`
/// (DMN-076) so a caller can gate UI on what actually works instead of
/// discovering it by hitting `UNIMPLEMENTED`. Append a value here in the same
/// change that ships the matching capability; never remove or rename a value
/// once released — older platform builds may still be checking for it.
pub const CAPABILITIES: &[&str] = &["sources", "credentials"];

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

    /// Upgrade an app the caller owns (DMN-053): `spec` is its id or custom
    /// name, optionally `@version`. Cloning happens with the daemon's own git
    /// credentials, like an install over this API.
    pub async fn upgrade(
        self: &Arc<Self>,
        ctx: UserContext,
        spec: String,
    ) -> Result<pkg::UpgradeOutcome> {
        self.blocking(move |s| pkg::upgrade(&s.config, &ctx, &spec))
            .await
    }

    /// Install from a registry spec or directly from a git URL (mirrors the
    /// CLI's dispatch). Without `license_ack` a repository shipping a
    /// LICENSE returns the typed [`pkg::LicenseRequired`] error — REST
    /// serializes it structurally so clients (the CLI over the unix socket,
    /// the platform UI) can render their own consent dialog and retry.
    #[allow(clippy::too_many_arguments)]
    pub async fn install(
        self: &Arc<Self>,
        ctx: UserContext,
        spec: String,
        source: Option<String>,
        name: Option<String>,
        branch: Option<String>,
        tag: Option<String>,
        license_ack: bool,
        image_choice: Option<crate::daemon::apps::ImageSource>,
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
                let report = pkg::install_from_git(
                    &s.config,
                    &ctx,
                    &spec,
                    git_ref,
                    name.as_deref(),
                    license_ack,
                    image_choice,
                )?;
                return Ok(pkg::InstallOutcome::App(report));
            }
            if branch.is_some() || tag.is_some() {
                anyhow::bail!(
                    "branch and tag are only used for a direct repository install (a git URL as the spec)"
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
            )
        })
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
    ) -> Result<String> {
        self.blocking(move |s| s.manager.logs(&ctx, &id, tail))
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
    /// after an in-process edit.
    pub async fn set_app_settings(
        self: &Arc<Self>,
        ctx: UserContext,
        id: String,
        values: pkg::settings::SettingValues,
    ) -> Result<()> {
        self.blocking(move |s| {
            let (file, _, config_dir) = s.settings_of(&ctx, &id)?;
            let defs = file.as_ref().map(|f| f.settings.as_slice()).unwrap_or(&[]);
            values.validate_against(defs)?;
            std::fs::create_dir_all(&config_dir)
                .with_context(|| format!("cannot create directory {}", config_dir.display()))?;
            values.save(&config_dir)
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

    // ── Registry sources & credentials (DMN-083/084) — pushed by the
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
        token: String,
        username: Option<String>,
        app: Option<String>,
    ) -> Result<pkg::auth::Credential> {
        self.blocking(move |_s| {
            let mut auth = pkg::auth::GitAuth::load_with(pkg::sources::Scope::System)?;
            let credential = auth
                .add(
                    kind,
                    &target,
                    pkg::auth::Method::Token { token },
                    username,
                    app,
                )?
                .clone();
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

    pub async fn list_directory(
        self: &Arc<Self>,
        ctx: UserContext,
        path: String,
        include_hidden: bool,
    ) -> Result<files::Listing> {
        self.blocking(move |_| {
            files::require_root(&ctx)?;
            Ok(files::list_directory(&path, include_hidden)?)
        })
        .await
    }

    pub async fn stat_path(
        self: &Arc<Self>,
        ctx: UserContext,
        path: String,
    ) -> Result<(files::FileEntry, String)> {
        self.blocking(move |_| {
            files::require_root(&ctx)?;
            Ok(files::stat(&path)?)
        })
        .await
    }

    pub async fn create_directory(
        self: &Arc<Self>,
        ctx: UserContext,
        path: String,
        parents: bool,
    ) -> Result<files::FileEntry> {
        self.blocking(move |_| {
            files::require_root(&ctx)?;
            Ok(files::create_directory(&path, parents)?)
        })
        .await
    }

    pub async fn move_path(
        self: &Arc<Self>,
        ctx: UserContext,
        source: String,
        destination: String,
        overwrite: bool,
    ) -> Result<files::FileEntry> {
        self.blocking(move |_| {
            files::require_root(&ctx)?;
            Ok(files::move_path(&source, &destination, overwrite)?)
        })
        .await
    }

    pub async fn copy_path(
        self: &Arc<Self>,
        ctx: UserContext,
        source: String,
        destination: String,
        overwrite: bool,
    ) -> Result<(files::FileEntry, u64, u32)> {
        self.blocking(move |_| {
            files::require_root(&ctx)?;
            Ok(files::copy_path(&source, &destination, overwrite)?)
        })
        .await
    }

    pub async fn delete_paths(
        self: &Arc<Self>,
        ctx: UserContext,
        paths: Vec<String>,
        recursive: bool,
    ) -> Result<(u32, Vec<(String, String)>)> {
        self.blocking(move |_| {
            files::require_root(&ctx)?;
            Ok(files::delete_paths(&paths, recursive))
        })
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_archive(
        self: &Arc<Self>,
        ctx: UserContext,
        directory: String,
        names: Vec<String>,
        archive_path: String,
        format: files::ArchiveFormat,
    ) -> Result<(files::FileEntry, u64, u32)> {
        self.blocking(move |_| {
            files::require_root(&ctx)?;
            Ok(files::create_archive(
                &directory,
                &names,
                &archive_path,
                format,
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
        path: String,
        offset: u64,
    ) -> Result<(u64, tokio::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>)> {
        files::require_root(&ctx)?;
        let mut handle =
            tokio::task::spawn_blocking(move || files::ReadHandle::open(&path, offset))
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
        path: String,
        mode: Option<u32>,
        owner: Option<String>,
        group: Option<String>,
    ) -> Result<files::FileEntry> {
        self.blocking(move |_| {
            files::require_root(&ctx)?;
            Ok(files::set_attributes(
                &path,
                mode,
                owner.as_deref(),
                group.as_deref(),
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

    /// Open an upload sink on a worker thread; the mirror of
    /// [`Self::open_file_read`]. The returned sender feeds chunks in; the
    /// join handle resolves once the header, every chunk and the final
    /// atomic commit have all landed, or reports why they did not.
    pub async fn open_file_write(
        self: &Arc<Self>,
        ctx: UserContext,
        header: files::WriteHeader,
    ) -> Result<(
        tokio::sync::mpsc::Sender<Vec<u8>>,
        tokio::task::JoinHandle<Result<files::FileEntry>>,
    )> {
        files::require_root(&ctx)?;
        let mut handle = tokio::task::spawn_blocking(move || files::WriteHandle::open(&header))
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
