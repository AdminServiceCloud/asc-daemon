//! Per-app disk usage (DMN-035): what an installed app occupies on disk —
//! image, repository checkout, private data and custom volumes — against
//! its quota (`asc.settings.yaml` `quota.max_disk`) when one is set.

use std::fs;
use std::path::Path;

use anyhow::Result;
use tracing::warn;

use crate::daemon::config::Config;
use crate::daemon::docker;
use crate::daemon::pkg::{self, manifest::Manifest, settings};

use super::meta::{AppMeta, Runtime};
use super::store::AppStore;

/// One custom volume entry, resolved to where its bytes actually live.
pub struct VolumeUsage {
    /// The raw `asc.settings.yaml` volume entry.
    pub entry: String,
    /// Resolved host path, or the Docker volume name when it has not been
    /// created yet (no mountpoint to measure).
    pub path: String,
    /// `None` when the size could not be determined (e.g. a named volume
    /// that has not been created yet, or Docker is unreachable).
    pub bytes: Option<u64>,
    /// A Docker named volume — may be mounted by other apps too.
    pub shared: bool,
    /// Whether `bytes` is already included in [`DiskUsage::app_dir_bytes`]:
    /// true for private volumes (they live inside the app directory), false
    /// for host paths and named volumes (they live outside it).
    pub counted: bool,
}

/// Disk usage of one installed app.
pub struct DiskUsage {
    /// Everything under the app's directory (repository + data + config +
    /// any private volume folders) — what the quota measures.
    pub app_dir_bytes: u64,
    /// `meta.quota.disk_bytes`, if set.
    pub quota_bytes: Option<u64>,
    /// Docker image size, for Docker apps whose image has been pulled.
    /// `None` for non-Docker apps or when Docker cannot report it.
    pub image_bytes: Option<u64>,
    pub repository_bytes: u64,
    pub data_bytes: u64,
    /// Custom volumes only — the default private volume (mounted from the
    /// `data` folder with no host override) is covered by `data_bytes`.
    pub volumes: Vec<VolumeUsage>,
}

/// Compute [`DiskUsage`] for an installed app. Manifest/settings/Docker
/// failures degrade to missing figures (with a warning) rather than failing
/// the whole report — a broken registry link must not hide the app
/// directory sizes, which are always available from the filesystem alone.
pub fn usage(config: &Config, store: &AppStore, meta: &AppMeta) -> Result<DiskUsage> {
    let app_dir = store.app_dir(&meta.id)?;

    let located = match settings::locate_installed(config, meta, &app_dir) {
        Ok((manifest_dir, _)) => match Manifest::load(&manifest_dir) {
            Ok(manifest) => Some((manifest_dir, manifest)),
            Err(err) => {
                warn!(app = %meta.id, error = %format!("{err:#}"), "cannot load app manifest for disk usage");
                None
            }
        },
        Err(err) => {
            warn!(app = %meta.id, error = %format!("{err:#}"), "cannot locate app manifest for disk usage");
            None
        }
    };

    let image_bytes = match (&meta.runtime, &located) {
        (Runtime::Docker { .. }, Some((_, manifest))) => image_bytes_of(config, meta, manifest),
        _ => None,
    };
    let volumes = match &located {
        Some((manifest_dir, manifest)) => {
            volume_usages(config, meta, manifest_dir, manifest, &app_dir)
        }
        None => Vec::new(),
    };

    Ok(DiskUsage {
        app_dir_bytes: dir_size(&app_dir),
        quota_bytes: meta.quota.as_ref().and_then(|q| q.disk_bytes),
        image_bytes,
        repository_bytes: dir_size(&app_dir.join("repository")),
        data_bytes: dir_size(&app_dir.join("data")),
        volumes,
    })
}

fn image_bytes_of(config: &Config, meta: &AppMeta, manifest: &Manifest) -> Option<u64> {
    let image = manifest.runtime.image.as_deref()?;
    match docker::image_size(&config.docker, image) {
        Ok(size) => size,
        Err(err) => {
            warn!(app = %meta.id, error = %format!("{err:#}"), "cannot read image size");
            None
        }
    }
}

fn volume_usages(
    config: &Config,
    meta: &AppMeta,
    manifest_dir: &Path,
    manifest: &Manifest,
    app_dir: &Path,
) -> Vec<VolumeUsage> {
    let settings_file = match settings::SettingsFile::load_for(manifest_dir, manifest) {
        Ok(settings_file) => settings_file,
        Err(err) => {
            warn!(app = %meta.id, error = %format!("{err:#}"), "cannot load app settings for disk usage");
            return Vec::new();
        }
    };
    let inputs = match pkg::runtime_inputs(settings_file.as_ref(), &app_dir.join("config")) {
        Ok(inputs) => inputs,
        Err(err) => {
            warn!(app = %meta.id, error = %format!("{err:#}"), "cannot resolve app settings for disk usage");
            return Vec::new();
        }
    };

    let data_dir = app_dir.join("data");
    let mut out = Vec::with_capacity(inputs.volumes.len());
    for entry in &inputs.volumes {
        let kind = match pkg::classify_volume(entry, app_dir) {
            Ok(kind) => kind,
            Err(err) => {
                warn!(app = %meta.id, entry, error = %format!("{err:#}"), "cannot resolve volume entry");
                continue;
            }
        };
        let usage = match kind {
            pkg::VolumeKind::AppFolder(path) => {
                // The default private volume maps to the 'data' folder,
                // already reported on its own — skip it here.
                if path == data_dir {
                    continue;
                }
                VolumeUsage {
                    entry: entry.clone(),
                    path: path.display().to_string(),
                    bytes: Some(dir_size(&path)),
                    shared: false,
                    counted: true,
                }
            }
            pkg::VolumeKind::HostPath(path) => VolumeUsage {
                entry: entry.clone(),
                path: path.display().to_string(),
                bytes: Some(dir_size(&path)),
                shared: false,
                counted: false,
            },
            pkg::VolumeKind::Named(name) => {
                let mountpoint = match docker::volume_mountpoint(&config.docker, &name) {
                    Ok(mountpoint) => mountpoint,
                    Err(err) => {
                        warn!(app = %meta.id, volume = name, error = %format!("{err:#}"), "cannot inspect named volume");
                        None
                    }
                };
                let bytes = mountpoint.as_deref().map(dir_size);
                let path = mountpoint
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| name.clone());
                VolumeUsage {
                    entry: entry.clone(),
                    path,
                    bytes,
                    shared: true,
                    counted: false,
                }
            }
        };
        out.push(usage);
    }
    out
}

/// Recursive size of everything under `dir`, in bytes. Symlinks are never
/// followed (their own directory-entry size is not counted either) — a
/// crafted loop or a link outside the measured directory cannot inflate the
/// result or escape it. A missing directory reports zero.
///
/// `pub(crate)`: reused for the quick per-app disk figure in `asc stats`
/// (`AppManager::stats`, same module) and for the clone progress bar's total
/// (`pkg::clone`, a sibling module under `daemon`) — unlike [`usage`], it
/// skips the Docker image query and settings/volume resolution, cheap enough
/// to recompute on every stats refresh tick.
pub(crate) fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if let Ok(metadata) = entry.metadata() {
                total += metadata.len();
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_size_sums_nested_files_and_skips_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), [0u8; 100]).unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/b.txt"), [0u8; 250]).unwrap();
        assert_eq!(dir_size(dir.path()), 350);

        // A symlink (even to a large directory) contributes nothing.
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("big.bin"), [0u8; 9_000]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        assert_eq!(dir_size(dir.path()), 350);
    }

    #[test]
    fn dir_size_of_missing_directory_is_zero() {
        assert_eq!(dir_size(Path::new("/nonexistent/asc-disk-test")), 0);
    }
}
