//! Published ports per installed app (DMN-049): the host==container ports an
//! app exposes, resolved the same way the runtime publishes them — from the
//! `type: ports` settings of `asc.settings.yaml` with the app's chosen values
//! (defaults filled in). Feeds `asc ports` / `asc ls ports`.

use anyhow::Result;
use tracing::warn;

use crate::daemon::config::Config;
use crate::daemon::docker::PortProtocol;
use crate::daemon::pkg::{self, manifest::Manifest, settings};

use super::meta::AppMeta;
use super::store::AppStore;

/// The host==container ports an app publishes, each with its transport.
///
/// Manifest/settings failures degrade to an empty list (with a warning) rather
/// than failing the whole ports report — a broken registry link must not hide
/// the other apps' ports. The values mirror what the container actually
/// publishes ([`pkg::runtime_inputs`]), so a stopped app still reports the
/// ports it will bind on the next start.
pub fn published(
    config: &Config,
    store: &AppStore,
    meta: &AppMeta,
) -> Result<Vec<(u16, PortProtocol)>> {
    let app_dir = store.app_dir(&meta.id)?;
    let manifest_dir = match settings::locate_installed(config, meta, &app_dir) {
        Ok((dir, _)) => dir,
        Err(err) => {
            warn!(app = %meta.id, error = %format!("{err:#}"), "cannot locate app manifest for ports");
            return Ok(Vec::new());
        }
    };
    let manifest = match Manifest::load(&manifest_dir) {
        Ok(manifest) => manifest,
        Err(err) => {
            warn!(app = %meta.id, error = %format!("{err:#}"), "cannot load app manifest for ports");
            return Ok(Vec::new());
        }
    };
    let settings_file = match settings::SettingsFile::load_for(&manifest_dir, &manifest) {
        Ok(settings_file) => settings_file,
        Err(err) => {
            warn!(app = %meta.id, error = %format!("{err:#}"), "cannot load app settings for ports");
            return Ok(Vec::new());
        }
    };
    let inputs = match pkg::runtime_inputs(settings_file.as_ref(), &app_dir.join("config")) {
        Ok(inputs) => inputs,
        Err(err) => {
            warn!(app = %meta.id, error = %format!("{err:#}"), "cannot resolve app settings for ports");
            return Ok(Vec::new());
        }
    };
    Ok(inputs.ports)
}
