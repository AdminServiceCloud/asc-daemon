//! Apply the current setting values to an app's runtime (DMN-017).
//!
//! A Docker container's environment is fixed at creation, so changed
//! settings can only land through a recreate. [`apply_settings`] runs while
//! the app is **stopped** (before a start, in the middle of a restart): when
//! the desired env has drifted from the container's actual one — or the
//! container is missing altogether — the container is recreated from the
//! current manifest, settings and quota, the same way an upgrade
//! re-provisions it. App data lives in volumes and survives the recreate.

use std::path::{Path, PathBuf};

use anyhow::Result;
use tracing::{info, warn};

use super::install::{load_quota, provision, settings_env_pairs};
use super::manifest::Manifest;
use super::settings::{SettingsFile, locate_installed};
use crate::daemon::apps::meta::{AppMeta, Quota, Runtime};
use crate::daemon::config::Config;
use crate::daemon::docker;

/// Bring a stopped Docker app's container in line with the current settings.
/// Non-Docker runtimes are untouched. A failure to compute the desired state
/// (unreadable manifest, missing registry source) is logged and does not
/// block the start — availability wins; a failed recreate is an error.
pub fn apply_settings(config: &Config, meta: &AppMeta, app_dir: &Path) -> Result<()> {
    let Runtime::Docker { container } = &meta.runtime else {
        return Ok(());
    };
    let desired = match Desired::load(config, meta, app_dir) {
        Ok(desired) => desired,
        Err(err) => {
            warn!(app = %meta.id, error = %format!("{err:#}"),
                "cannot compute the desired env, starting the container as is");
            return Ok(());
        }
    };
    // Every desired KEY=value must be present verbatim. The actual env also
    // carries the image's own variables, hence subset — not equality; a
    // variable dropped from the desired set lingers until the next drift.
    if let Some(actual) = docker::container_env(&config.docker, container)?
        && desired.env.iter().all(|pair| actual.contains(pair))
    {
        return Ok(());
    }
    info!(app = %meta.id, "settings changed (or container missing), recreating the container");
    docker::remove(&config.docker, container)?;
    provision(
        &desired.manifest,
        &meta.id,
        app_dir,
        &desired.manifest_dir,
        &config.docker,
        desired.quota.as_ref(),
        desired.settings.as_ref(),
    )?;
    Ok(())
}

/// Everything a recreate needs, loaded from the installed app's repository:
/// the manifest, the settings file, the normalized quota and the desired
/// container env.
struct Desired {
    manifest: Manifest,
    manifest_dir: PathBuf,
    settings: Option<SettingsFile>,
    quota: Option<Quota>,
    env: Vec<String>,
}

impl Desired {
    fn load(config: &Config, meta: &AppMeta, app_dir: &Path) -> Result<Self> {
        let (manifest_dir, _) = locate_installed(config, meta, app_dir)?;
        let manifest = Manifest::load(&manifest_dir)?;
        let settings = SettingsFile::load_for(&manifest_dir, &manifest)?;
        let quota = load_quota(settings.as_ref())?;
        let env = settings_env_pairs(settings.as_ref(), &app_dir.join("config"))?
            .into_iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        Ok(Self {
            manifest,
            manifest_dir,
            settings,
            quota,
            env,
        })
    }
}
