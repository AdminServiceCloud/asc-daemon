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
    RemoveOnDrop, clone_repository, enforce_install_policy, manifest_dir, parse_spec, provision,
};
use super::manifest::Manifest;
use super::registry::RegistryClient;
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

    let resolved = RegistryClient::new(config)?.resolve(id)?;
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
    let new_manifest_dir = manifest_dir(&new_dir, resolved.entry.source.path.as_deref())?;
    let manifest = Manifest::load(&new_manifest_dir)?;
    enforce_install_policy(config, ctx, &manifest, id)?;

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
    let manifest_sub = resolved.entry.source.path.as_deref();
    let runtime = match provision(
        &manifest,
        id,
        &app_dir,
        &manifest_dir(&repo_dir, manifest_sub)?,
        &config.docker,
    ) {
        Ok(runtime) => runtime,
        Err(err) => {
            rollback(config, id, &app_dir, &repo_dir, &old_dir, manifest_sub);
            return Err(err.context(format!(
                "upgrade of '{id}' failed, previous version restored"
            )));
        }
    };
    let _ = fs::remove_dir_all(&old_dir);

    let from = meta.version.clone();
    let mut meta = meta;
    meta.name = manifest.title.clone().unwrap_or_else(|| id.to_string());
    meta.version = Some(cloned_tag.clone());
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
    manifest_sub: Option<&str>,
) {
    let _ = fs::remove_dir_all(repo_dir);
    if let Err(err) = fs::rename(old_dir, repo_dir) {
        warn!(app = id, error = %err, "rollback: cannot restore the previous repository");
        return;
    }
    let restore = manifest_dir(repo_dir, manifest_sub)
        .and_then(|dir| Manifest::load(&dir))
        .and_then(|manifest| {
            provision(
                &manifest,
                id,
                app_dir,
                &manifest_dir(repo_dir, manifest_sub)?,
                &config.docker,
            )
        });
    if let Err(err) = restore {
        warn!(app = id, error = %format!("{err:#}"), "rollback: cannot re-provision the previous version");
    }
}
