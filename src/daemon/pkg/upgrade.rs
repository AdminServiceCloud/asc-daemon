//! Upgrade flow (DMN-003): versions are git tags, upgrading checks out a
//! new tag. The package is resolved again, the requested (or latest) tag is
//! cloned **next to** the current repository, the manifest is validated, and
//! only then the repository is swapped and the runtime re-provisioned — any
//! failure before the swap leaves the installed app untouched, and a
//! provisioning failure rolls back to the previous version.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use super::install::{
    RemoveOnDrop, clone_repository, enforce_install_policy, load_quota, locate_manifest,
    parse_spec, provision,
};
use super::manifest::Manifest;
use super::registry::RegistryClient;
use super::settings::{SettingValues, SettingsFile};
use crate::daemon::apps::meta::Runtime;
use crate::daemon::apps::{AppManager, RuntimeState, UserContext};
use crate::daemon::config::Config;
use crate::daemon::docker;
use crate::daemon::i18n::{Msg, tf2};

#[derive(Debug)]
pub enum UpgradeOutcome {
    Upgraded {
        id: String,
        from: Option<String>,
        to: String,
    },
    UpToDate {
        id: String,
        version: String,
    },
}

/// Upgrade `name` (to the registry's latest tag) or `name@version`.
/// The app must be stopped.
pub fn upgrade(config: &Config, ctx: &UserContext, spec: &str) -> Result<UpgradeOutcome> {
    let (id, requested_version) = parse_spec(spec);
    let manager = AppManager::new(config);
    // Ownership check plus live state: only stopped apps are upgraded.
    let status = manager.status(ctx, id)?;
    if status.state == RuntimeState::Running {
        bail!(tf2(Msg::PkgUpgradeStopFirst, id, id));
    }
    let meta = status.meta;

    // Stack apps record their origin as `stack/app` in meta.package;
    // plain apps resolve by their own id.
    let package_spec = meta.package.clone().unwrap_or_else(|| id.to_string());
    let (package, stack_app) = match package_spec.split_once('/') {
        Some((package, app)) => (package, Some(app)),
        None => (package_spec.as_str(), None),
    };

    // When several sources provide the package, prefer the one the app was
    // installed from (meta.source = "name:git-url"); fall back to priority.
    let installed_from = meta
        .source
        .as_deref()
        .and_then(|s| s.split_once(':'))
        .map(|(name, _)| name);
    let resolved = RegistryClient::new(config)?.resolve_preferring(package, installed_from)?;
    let Some(version) = requested_version
        .map(str::to_string)
        .or_else(|| resolved.entry.latest.clone())
    else {
        bail!(
            "registry entry for '{id}' has no latest version; run: asc app upgrade {id}@<version>"
        );
    };
    // The installed version is the tag that was actually checked out
    // (`1.2.0` or `v1.2.0`), so compare against both spellings.
    if let Some(current) = &meta.version
        && (*current == version || *current == format!("v{version}"))
    {
        return Ok(UpgradeOutcome::UpToDate {
            id: id.to_string(),
            version: current.clone(),
        });
    }

    let store = manager.store();
    let app_dir = store.app_dir(id)?;
    let repo_dir = app_dir.join("repository");
    let new_dir = app_dir.join("repository.new");
    let old_dir = app_dir.join("repository.old");
    // Leftovers of an interrupted upgrade must not fail this one.
    let _ = fs::remove_dir_all(&new_dir);
    let _ = fs::remove_dir_all(&old_dir);

    let mut cleanup = RemoveOnDrop {
        path: new_dir.clone(),
        armed: true,
    };
    let cloned_tag = clone_repository(&resolved.entry.source.git, Some(&version), &new_dir)?
        .expect("a version was requested, so a tag was checked out");
    let entry_path = resolved.entry.source.path.as_deref();
    let (new_manifest_dir, stack) = locate_manifest(&new_dir, entry_path, stack_app)?;
    let mut manifest = Manifest::load(&new_manifest_dir)?;
    if let Some(stack) = &stack {
        manifest.merge_stack_env(&stack.env);
    }
    enforce_install_policy(config, ctx, &manifest, id)?;
    let settings = SettingsFile::load_for(&new_manifest_dir, &manifest)?;
    let quota = load_quota(settings.as_ref())?;

    // Point of no return: swap the repository, keeping the old one around
    // until the new runtime is provisioned.
    fs::rename(&repo_dir, &old_dir)
        .with_context(|| format!("cannot move aside {}", repo_dir.display()))?;
    if let Err(err) = fs::rename(&new_dir, &repo_dir) {
        let _ = fs::rename(&old_dir, &repo_dir);
        return Err(err)
            .with_context(|| format!("cannot move new version into {}", repo_dir.display()));
    }
    cleanup.disarm();

    // Tear down the old runtime (the container name is reused) and build the
    // new one; on failure restore the previous repository and runtime.
    teardown_runtime(config, &meta.runtime)?;
    let runtime = match provision(
        &manifest,
        id,
        &app_dir,
        &locate_manifest(&repo_dir, entry_path, stack_app)?.0,
        &config.docker,
        quota.as_ref(),
        settings.as_ref().and_then(|s| s.start_command.as_deref()),
    ) {
        Ok(runtime) => runtime,
        Err(err) => {
            rollback(
                config, id, &app_dir, &repo_dir, &old_dir, entry_path, stack_app,
            );
            return Err(err.context(format!(
                "upgrade of '{id}' failed, previous version restored"
            )));
        }
    };
    let _ = fs::remove_dir_all(&old_dir);

    // New settings keys get their defaults; values the user chose survive.
    if let Some(settings) = &settings
        && !settings.settings.is_empty()
    {
        let config_dir = app_dir.join("config");
        let mut values = SettingValues::load(&config_dir).unwrap_or_default();
        values.merge_defaults(&settings.settings);
        if let Err(err) = values.save(&config_dir) {
            warn!(app = id, error = %format!("{err:#}"), "cannot refresh setting defaults");
        }
    }

    let from = meta.version.clone();
    let mut meta = meta;
    meta.name = manifest.title.clone().unwrap_or_else(|| id.to_string());
    meta.version = Some(cloned_tag.clone());
    meta.quota = quota;
    meta.runtime = runtime;
    store.save(&meta)?;
    info!(app = id, from = %from.as_deref().unwrap_or("-"), to = %cloned_tag, "app upgraded");
    Ok(UpgradeOutcome::Upgraded {
        id: id.to_string(),
        from,
        to: cloned_tag,
    })
}

/// Remove the runtime objects the previous version created. Process apps
/// have nothing to tear down; systemd/docker units are recreated by
/// `provision` from the new manifest.
fn teardown_runtime(config: &Config, runtime: &Runtime) -> Result<()> {
    match runtime {
        Runtime::Docker { container } => docker::remove(&config.docker, container)
            .context("cannot remove the old container before upgrade"),
        Runtime::Systemd { .. } | Runtime::Process { .. } => Ok(()),
    }
}

/// Best-effort restore after a failed provisioning: put the old repository
/// back and re-provision the previous runtime from its manifest.
fn rollback(
    config: &Config,
    id: &str,
    app_dir: &Path,
    repo_dir: &Path,
    old_dir: &Path,
    entry_path: Option<&str>,
    stack_app: Option<&str>,
) {
    let _ = fs::remove_dir_all(repo_dir);
    if let Err(err) = fs::rename(old_dir, repo_dir) {
        warn!(app = id, error = %err, "rollback: cannot restore the previous repository");
        return;
    }
    let restore = locate_manifest(repo_dir, entry_path, stack_app).and_then(|(dir, stack)| {
        let mut manifest = Manifest::load(&dir)?;
        if let Some(stack) = &stack {
            manifest.merge_stack_env(&stack.env);
        }
        let settings = SettingsFile::load_for(&dir, &manifest)?;
        let quota = load_quota(settings.as_ref())?;
        provision(
            &manifest,
            id,
            app_dir,
            &dir,
            &config.docker,
            quota.as_ref(),
            settings.as_ref().and_then(|s| s.start_command.as_deref()),
        )
    });
    if let Err(err) = restore {
        warn!(app = id, error = %format!("{err:#}"), "rollback: cannot re-provision the previous version");
    }
}
