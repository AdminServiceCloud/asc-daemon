//! Apply the current setting values to an app's runtime (DMN-017, DMN-030).
//!
//! A Docker container's configuration is fixed at creation, so changed
//! settings — env, published ports, volumes, the quota override or the
//! start command — can only land through a recreate. [`apply_settings`]
//! runs while the app is **stopped** (before a start, in the middle of a
//! restart): when the desired configuration has drifted from the
//! container's actual one — or the container is missing altogether — the
//! container is recreated from the current manifest and settings, the same
//! way an upgrade re-provisions it. App data lives in volumes and survives
//! the recreate.

use std::path::{Path, PathBuf};

use anyhow::Result;
use tracing::{info, warn};

use super::install::{interpolate_env, load_quota, provision, runtime_inputs, volume_bind};
use super::manifest::Manifest;
use super::settings::{SettingsFile, locate_installed};
use crate::daemon::apps::meta::{AppMeta, Quota, Runtime};
use crate::daemon::config::Config;
use crate::daemon::docker;

/// Bring a stopped Docker app's container in line with the current settings.
/// Non-Docker runtimes are untouched. Returns `true` when the container was
/// recreated and `meta` (runtime, quota) was updated — the caller persists
/// it. A failure to compute the desired state (unreadable manifest, missing
/// registry source) is logged and does not block the start — availability
/// wins; a failed recreate is an error.
pub fn apply_settings(config: &Config, meta: &mut AppMeta, app_dir: &Path) -> Result<bool> {
    let Runtime::Docker { container } = &meta.runtime else {
        return Ok(false);
    };
    let container = container.clone();
    let desired = match Desired::load(config, meta, app_dir) {
        Ok(desired) => desired,
        Err(err) => {
            warn!(app = %meta.id, error = %format!("{err:#}"),
                "cannot compute the desired configuration, starting the container as is");
            return Ok(false);
        }
    };
    if let Some(actual) = docker::container_applied(&config.docker, &container)?
        && desired.matches(&actual)
    {
        return Ok(false);
    }
    info!(app = %meta.id, "settings changed (or container missing), recreating the container");
    docker::remove(&config.docker, &container)?;
    let runtime = provision(
        &desired.manifest,
        &meta.id,
        meta.uuid.as_deref(),
        app_dir,
        &desired.manifest_dir,
        &config.docker,
        desired.quota.as_ref(),
        desired.settings.as_ref(),
    )?;
    // Keep meta truthful for `asc app info`: the quota may have been
    // overridden in the settings editor.
    meta.quota = desired.quota;
    meta.runtime = runtime;
    Ok(true)
}

/// Everything a recreate needs plus the comparable desired configuration,
/// loaded from the installed app's repository and settings.
struct Desired {
    manifest: Manifest,
    manifest_dir: PathBuf,
    settings: Option<SettingsFile>,
    quota: Option<Quota>,
    /// `KEY=value` pairs the container env must contain.
    env: Vec<String>,
    /// Published port keys (`"27015/tcp"`), sorted.
    ports: Vec<String>,
    /// Volume binds as the Engine sees them, sorted.
    binds: Vec<String>,
    /// The interpolated start command, when one applies.
    command: Option<String>,
}

impl Desired {
    fn load(config: &Config, meta: &AppMeta, app_dir: &Path) -> Result<Self> {
        let (manifest_dir, _) = locate_installed(config, meta, app_dir)?;
        let manifest = Manifest::load(&manifest_dir)?;
        let settings = SettingsFile::load_for(&manifest_dir, &manifest)?;
        let config_dir = app_dir.join("config");
        let quota = load_quota(settings.as_ref(), &config_dir)?;
        let inputs = runtime_inputs(settings.as_ref(), &config_dir)?;
        let command = inputs
            .start_command
            .as_deref()
            .map(|c| interpolate_env(c, &inputs.env))
            .transpose()?;
        let env = inputs
            .env
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        let mut ports: Vec<String> = inputs
            .ports
            .iter()
            .flat_map(|(port, protocol)| {
                protocol
                    .transports()
                    .iter()
                    .map(move |transport| format!("{port}/{transport}"))
            })
            .collect();
        ports.sort();
        ports.dedup();
        // The manifest is validated at install time: a docker-type manifest
        // always has runtime.image (Manifest::validate).
        let owner = manifest
            .runtime
            .image
            .as_deref()
            .map(|image| {
                let auth = super::install::registry_auth_for(
                    image,
                    &[Some(meta.id.as_str()), meta.uuid.as_deref()],
                );
                docker::ensure_pulled(&config.docker, image, auth.as_ref())?;
                docker::image_uid_gid(&config.docker, image)
            })
            .transpose()?
            .flatten();
        let mut binds = inputs
            .volumes
            .iter()
            .map(|volume| volume_bind(volume, app_dir, owner))
            .collect::<Result<Vec<String>>>()?;
        binds.sort();
        Ok(Self {
            manifest,
            manifest_dir,
            settings,
            quota,
            env,
            ports,
            binds,
            command,
        })
    }

    /// Whether the container already carries this configuration. Env is a
    /// subset check (the image adds its own variables); ports and binds are
    /// exact (we set them); a cleared start_command is not detectable (the
    /// image's own cmd is unknown) and applies with the next real drift.
    fn matches(&self, actual: &docker::AppliedConfig) -> bool {
        // 1 core = 1e9 NanoCpus, same scale as the create spec.
        let nano_cpus = self
            .quota
            .as_ref()
            .and_then(|q| q.cpu_cores)
            .map(|cores| (cores * 1_000_000_000.0) as i64)
            .unwrap_or(0);
        let memory = self
            .quota
            .as_ref()
            .and_then(|q| q.ram_bytes)
            .map(|bytes| bytes as i64)
            .unwrap_or(0);
        self.env.iter().all(|pair| actual.env.contains(pair))
            && self.ports == actual.ports
            && self.binds == actual.binds
            && nano_cpus == actual.nano_cpus
            && memory == actual.memory
            && match &self.command {
                Some(command) => actual.cmd.as_deref() == Some(std::slice::from_ref(command)),
                None => true,
            }
    }
}
