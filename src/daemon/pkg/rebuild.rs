//! Rebuild: pull or build an app's images afresh and force-recreate its
//! containers, without moving it to another version.
//!
//! An upgrade moves an app to another ref; a rebuild keeps the ref it
//! already follows and only (optionally) fetches it again — the "same ref,
//! git pull, rebuild" an operator wants after pushing a fix to the branch
//! the app tracks, or after a base image got a security update. What it
//! does depends on the runtime:
//!
//! - **docker compose** — [`super::compose_update`]: the whole project
//!   (fetch, check, pull/build while the old containers run, recreate,
//!   rollback on failure), or a single service (pull/build it, recreate it
//!   alone with `--no-deps`).
//! - **docker** — fetch the tracked branch if it moved (stop → upgrade →
//!   start, like a manual upgrade), then pull the image under whatever tag
//!   it has — not only `latest`, unlike a repull — or rebuild it with a
//!   fresh base image, and recreate the container.
//! - **systemd / process** — nothing to rebuild: [`NotRebuildable`].
//!
//! The app keeps its desired and actual state: a running app runs again
//! afterwards, a stopped one stays stopped with its containers recreated.
//! The whole run holds the app's operation lock, so it never interleaves
//! with an upgrade or another rebuild of the same app.

use std::path::Path;

use anyhow::{Result, bail};
use tracing::info;

use super::gitref::{head_commit, ls_remote, remote_commit};
use super::install::{VersionSpec, build_app_image, effective_image_ref, registry_auth_for};
use super::upgrade::{UpgradeOutcome, UpgradeRef, upgrade_locked};
use crate::daemon::apps::meta::{AppMeta, Runtime};
use crate::daemon::apps::{AppManager, RuntimeState, UserContext, oplock};
use crate::daemon::config::Config;
use crate::daemon::docker;
use crate::daemon::exec;
use crate::daemon::i18n::{Msg, t, tf2};
use crate::daemon::progress::{self, InstallReporter};

/// Refusal of a rebuild: the app has no image to rebuild. Rendered as
/// `FAILED_PRECONDITION` / REST 409.
#[derive(Debug)]
pub struct NotRebuildable {
    pub app: String,
    pub kind: &'static str,
}

impl std::fmt::Display for NotRebuildable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&tf2(Msg::AppNotRebuildable, &self.app, self.kind))
    }
}

impl std::error::Error for NotRebuildable {}

/// What a rebuild did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebuildOutcome {
    pub id: String,
    /// Services rebuilt and recreated: every service of a compose project
    /// (or the one asked for), the app id itself for a docker app.
    pub services: Vec<String>,
    pub from_commit: Option<String>,
    pub to_commit: Option<String>,
    /// `pull_source` moved the repository to another commit.
    pub source_updated: bool,
    /// The app (or the service) runs after the rebuild.
    pub running: bool,
}

/// Rebuild the app `reference` (id or custom name) the caller may manage.
/// `service` narrows a compose app's rebuild to one service and cannot be
/// combined with `pull_source`, which fetches the ref the app follows first.
pub fn rebuild(
    config: &Config,
    ctx: &UserContext,
    reference: &str,
    service: Option<&str>,
    pull_source: bool,
    report: Option<&dyn InstallReporter>,
) -> Result<RebuildOutcome> {
    if service.is_some() && pull_source {
        return Err(exec::invalid(t(Msg::RebuildServiceWithPull)));
    }
    let manager = AppManager::new(config);
    let meta = manager.get_authorized(ctx, reference)?;
    match &meta.runtime {
        Runtime::Systemd { .. } | Runtime::Process { .. } => {
            return Err(NotRebuildable {
                app: meta.id.clone(),
                kind: meta.runtime.kind(),
            }
            .into());
        }
        Runtime::Docker { .. } if service.is_some() => {
            return Err(exec::invalid(tf2(
                Msg::AppServicesComposeOnly,
                &meta.id,
                meta.runtime.kind(),
            )));
        }
        Runtime::Docker { .. } | Runtime::Compose { .. } => {}
    }
    let app_dir = manager.store().app_dir(&meta.id)?;
    let _lock = oplock::acquire(&app_dir, &meta.id)?;
    let outcome = if matches!(meta.runtime, Runtime::Compose { .. }) {
        super::compose_update::rebuild(config, ctx, meta, &app_dir, service, pull_source, report)?
    } else {
        rebuild_docker(config, ctx, &manager, meta, &app_dir, pull_source, report)?
    };
    info!(
        app = %outcome.id,
        services = %outcome.services.join(","),
        source_updated = outcome.source_updated,
        running = outcome.running,
        "app rebuilt"
    );
    Ok(outcome)
}

/// The docker path of [`rebuild`].
fn rebuild_docker(
    config: &Config,
    ctx: &UserContext,
    manager: &AppManager,
    mut meta: AppMeta,
    app_dir: &Path,
    pull_source: bool,
    report: Option<&dyn InstallReporter>,
) -> Result<RebuildOutcome> {
    let id = meta.id.clone();
    let repo = app_dir.join("repository");
    let was_running = manager.status(ctx, &id)?.state == RuntimeState::Running;
    let from_commit = head_commit(&repo);

    // 1. The source, when the ref the app follows has moved on. The app's
    // data lives outside `repository/`, so the usual clone-beside upgrade is
    // safe here; it wants the app stopped, which is done on the container
    // directly so the app's desired state is never touched.
    let mut source_updated = false;
    if pull_source && moved_upstream(&meta, ctx, from_commit.as_deref())? {
        if was_running {
            if let Some(report) = report {
                report.line("stopping the app to update its source");
            }
            docker::stop(&config.docker, container_of(&meta)?)?;
        }
        let upgraded = upgrade_locked(
            config,
            ctx,
            manager,
            meta.clone(),
            VersionSpec::Latest,
            &UpgradeRef::Spec,
            report,
        );
        match upgraded {
            Ok(UpgradeOutcome::Upgraded { .. }) => source_updated = true,
            Ok(UpgradeOutcome::UpToDate { .. }) => {}
            Err(err) => {
                // The upgrade restored the previous version; bring it back
                // to the state it was in.
                if was_running
                    && let Ok(Some(restored)) = manager.store().get(&id)
                    && let Ok(container) = container_of(&restored)
                    && let Err(start_err) = docker::start(&config.docker, container)
                {
                    tracing::warn!(app = %id, error = %format!("{start_err:#}"),
                        "cannot start the app again after a failed source update");
                }
                return Err(err);
            }
        }
        meta = manager.get_authorized(ctx, &id)?;
    }
    progress::ensure_not_cancelled(report)?;

    // 2. The image, while the container (if it still runs) keeps running:
    // a prebuilt one is pulled under whatever tag it has, a local build
    // is redone on a freshly pulled base image.
    let Runtime::Docker { image_source, .. } = &meta.runtime else {
        bail!("app '{id}' is not a docker app");
    };
    let (manifest_dir, _) = super::settings::locate_installed(config, &meta, app_dir)?;
    let (manifest, _) = super::dockerfile::resolve_installed(&meta, &manifest_dir)?;
    let Some(image) = effective_image_ref(&manifest, *image_source, &id) else {
        bail!("the manifest of '{id}' names no image");
    };
    if manifest.runtime.image.as_deref() == Some(image.as_str()) {
        let auth = registry_auth_for(&image, &[Some(id.as_str()), meta.uuid.as_deref()]);
        docker::pull_image(&config.docker, &image, auth.as_ref(), report)?;
    } else {
        build_app_image(&manifest, &config.docker, &manifest_dir, &id, true, report)?;
    }
    progress::ensure_not_cancelled(report)?;

    // 3. The container, recreated onto that image; started again only if it
    // was running.
    if let Some(report) = report {
        report.line(&format!("recreating the container of '{id}'"));
    }
    super::refresh::recreate(config, &mut meta, app_dir, report)?;
    manager.store().save(&meta)?;
    if was_running {
        docker::start(&config.docker, container_of(&meta)?)?;
    }
    Ok(RebuildOutcome {
        services: vec![id.clone()],
        id,
        from_commit,
        to_commit: head_commit(&repo),
        source_updated,
        running: was_running,
    })
}

fn container_of(meta: &AppMeta) -> Result<&str> {
    match &meta.runtime {
        Runtime::Docker { container, .. } => Ok(container),
        other => bail!("app '{}' is not a docker app ({})", meta.id, other.kind()),
    }
}

/// Whether the ref a docker app follows points somewhere else upstream —
/// only a moving ref can: the branch the app tracks, or the default branch
/// of an untagged repository a direct install follows. A tag-pinned app
/// (every registry install) has nothing to fetch; a repository that gained
/// tags since is left to `asc upgrade`, which would move the app onto one.
fn moved_upstream(meta: &AppMeta, ctx: &UserContext, installed: Option<&str>) -> Result<bool> {
    let Some(git_url) = meta.source.as_deref().and_then(|s| s.strip_prefix("git:")) else {
        return Ok(false);
    };
    let refname = match (&meta.branch, &meta.version) {
        (Some(branch), _) => format!("refs/heads/{branch}"),
        (None, None) if ls_remote(git_url, ctx)?.latest_tag().is_none() => "HEAD".to_string(),
        _ => return Ok(false),
    };
    let upstream = remote_commit(git_url, ctx, &refname)?;
    Ok(upstream.is_some() && upstream.as_deref() != installed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::apps::meta::{DesiredState, Owner};

    fn sample(runtime: Runtime) -> AppMeta {
        AppMeta {
            id: "demo".into(),
            uuid: None,
            name: "demo".into(),
            custom_name: None,
            owner: Owner {
                uid: 0,
                name: "root".into(),
            },
            version: None,
            source: None,
            branch: None,
            repo_path: None,
            package: None,
            install_method: None,
            desired_state: DesiredState::Stopped,
            setup_skipped: false,
            quota: None,
            runtime,
        }
    }

    fn manager(dir: &Path) -> (Config, UserContext) {
        let mut config = Config::default();
        config.daemon.apps_dir = dir.to_path_buf();
        config.daemon.data_dir = dir.join(".data");
        let ctx = UserContext {
            uid: 0,
            name: "root".into(),
            is_root: true,
        };
        (config, ctx)
    }

    #[test]
    fn native_apps_have_nothing_to_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let (config, ctx) = manager(dir.path());
        let store = crate::daemon::apps::AppStore::new(dir.path());
        store
            .save(&sample(Runtime::Process {
                command: "true".into(),
                args: vec![],
            }))
            .unwrap();
        let err = rebuild(&config, &ctx, "demo", None, true, None).unwrap_err();
        let refusal = err
            .downcast_ref::<NotRebuildable>()
            .unwrap_or_else(|| panic!("got: {err:#}"));
        assert_eq!(refusal.kind, "process");
    }

    #[test]
    fn a_service_is_rebuilt_from_the_source_it_has() {
        let dir = tempfile::tempdir().unwrap();
        let (config, ctx) = manager(dir.path());
        let err = rebuild(&config, &ctx, "demo", Some("web"), true, None).unwrap_err();
        assert!(matches!(
            err.downcast_ref::<exec::ModuleError>(),
            Some(exec::ModuleError::Invalid(_))
        ));
    }

    #[test]
    fn only_a_compose_app_has_services() {
        let dir = tempfile::tempdir().unwrap();
        let (config, ctx) = manager(dir.path());
        let store = crate::daemon::apps::AppStore::new(dir.path());
        store
            .save(&sample(Runtime::Docker {
                container: "asc-demo".into(),
                image_source: None,
            }))
            .unwrap();
        let err = rebuild(&config, &ctx, "demo", Some("web"), false, None).unwrap_err();
        assert!(matches!(
            err.downcast_ref::<exec::ModuleError>(),
            Some(exec::ModuleError::Invalid(_))
        ));
    }

    #[test]
    fn a_tag_pinned_or_registry_app_has_nothing_upstream_to_move_to() {
        let ctx = UserContext {
            uid: 0,
            name: "root".into(),
            is_root: true,
        };
        let mut meta = sample(Runtime::Docker {
            container: "asc-demo".into(),
            image_source: None,
        });
        // No git source at all (a registry install): never fetched.
        meta.source = Some("official:https://example.invalid/demo".into());
        assert!(!moved_upstream(&meta, &ctx, Some("abc")).unwrap());
        // A direct install pinned to a tag: the tag does not move.
        meta.source = Some("git:https://example.invalid/demo".into());
        meta.version = Some("v1.0.0".into());
        assert!(!moved_upstream(&meta, &ctx, Some("abc")).unwrap());
    }
}
