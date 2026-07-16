//! Instance cloning (DMN-019): `asc app clone <id>` — a full copy of an
//! installed app (repository, config, data) under a new id, then a runtime
//! freshly provisioned from the copy. Docker containers, systemd units and
//! processes cannot be copied, only recreated from the copied manifest and
//! settings — this is why cloning re-runs the install-time provisioning
//! step instead of touching the runtime at all.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use tracing::info;

use super::install::{
    RemoveOnDrop, enforce_install_policy, load_quota, provision, validate_custom_name,
};
use super::manifest::Manifest;
use super::settings::{SettingsFile, locate_installed};
use crate::daemon::apps::disk;
use crate::daemon::apps::meta::{AppMeta, DesiredState, Owner};
use crate::daemon::apps::{AppStore, UserContext};
use crate::daemon::config::Config;

/// Copy `src` into `dst` file by file (directories created as needed),
/// calling `on_copied` after every file with the cumulative bytes copied so
/// far. Symlinks are skipped — never followed, never recreated — matching
/// [`disk::dir_size`]'s own traversal rules, so a clone cannot escape the
/// source tree or explode on a symlink loop.
fn copy_tree(
    src: &Path,
    dst: &Path,
    copied: &mut u64,
    on_copied: &mut dyn FnMut(u64),
) -> Result<()> {
    fs::create_dir_all(dst)
        .with_context(|| format!("cannot create directory {}", dst.display()))?;
    for entry in fs::read_dir(src).with_context(|| format!("cannot read {}", src.display()))? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if file_type.is_symlink() {
            continue;
        } else if file_type.is_dir() {
            copy_tree(&from, &to, copied, on_copied)?;
        } else {
            fs::copy(&from, &to)
                .with_context(|| format!("cannot copy {} to {}", from.display(), to.display()))?;
            *copied += entry.metadata().map(|m| m.len()).unwrap_or(0);
            on_copied(*copied);
        }
    }
    Ok(())
}

/// Clone `source` (already authorized — the caller resolves the reference
/// and checks ownership, e.g. via `AppManager::get_authorized`) into a new
/// instance: the next free `<id>-N` (DMN-033 numbering, the source's own id
/// as the base). `on_progress(copied, total)` reports the directory copy;
/// `total` is `0` when the source app directory could not be measured up
/// front (the copy still proceeds, just without a percentage). The clone
/// always starts stopped, regardless of the source's state.
pub fn clone_app(
    config: &Config,
    ctx: &UserContext,
    store: &AppStore,
    source: &AppMeta,
    custom_name: Option<&str>,
    mut on_progress: impl FnMut(u64, u64),
) -> Result<AppMeta> {
    if let Some(name) = custom_name {
        validate_custom_name(config, ctx, name)?;
    }
    let new_id = super::instance_id(store, &source.id)?;
    let source_dir = store.app_dir(&source.id)?;
    let dest_dir = store.app_dir(&new_id)?;

    fs::create_dir_all(&dest_dir)
        .with_context(|| format!("cannot create app directory {}", dest_dir.display()))?;
    let mut cleanup = RemoveOnDrop {
        path: dest_dir.clone(),
        armed: true,
    };

    // Only the three subdirectories below are actually copied — meta.json
    // (also under the app dir) is regenerated, not copied — so the total
    // must match that, not `disk::dir_size(&source_dir)` as a whole, or the
    // progress would stall short of 100%.
    let total: u64 = ["repository", "config", "data"]
        .iter()
        .map(|sub| disk::dir_size(&source_dir.join(sub)))
        .sum();
    let mut copied = 0u64;
    for sub in ["repository", "config", "data"] {
        let from = source_dir.join(sub);
        if from.is_dir() {
            copy_tree(&from, &dest_dir.join(sub), &mut copied, &mut |c| {
                on_progress(c, total)
            })?;
        }
    }
    // install_one's own invariant (config/ and data/ always exist) holds for
    // the clone too, even if the source never started and data/ is empty.
    for sub in ["config", "data"] {
        fs::create_dir_all(dest_dir.join(sub))
            .with_context(|| format!("cannot create {sub}/ in app directory"))?;
    }

    // The repository is a byte-identical copy, so the manifest sits at the
    // exact path it did in the source — locate_installed's fast path (an
    // asc.yaml right at the repository root) needs no registry lookup at
    // all; only monorepo/stack packages fall back to re-resolving it, using
    // the source's own package/source fields (the new id plays no part in
    // that resolution).
    let (manifest_dir, _) = locate_installed(config, source, &dest_dir)?;
    let manifest = Manifest::load(&manifest_dir)?;
    enforce_install_policy(config, ctx, &manifest, &new_id)?;

    let settings = SettingsFile::load_for(&manifest_dir, &manifest)?;
    // Recomputed from the copied config/settings.json rather than trusting
    // `source.quota`: a `$quota` override edited via `asc app settings`
    // only lands in meta.json on the app's next start (DMN-017/030), so
    // meta and settings.json can disagree until then — settings.json (just
    // copied verbatim) is the authoritative one.
    let quota = load_quota(settings.as_ref(), &dest_dir.join("config"))?;

    let runtime = provision(
        &manifest,
        &new_id,
        &dest_dir,
        &manifest_dir,
        &config.docker,
        quota.as_ref(),
        settings.as_ref(),
    )?;

    let meta = AppMeta {
        id: new_id.clone(),
        name: manifest.title.clone().unwrap_or_else(|| new_id.clone()),
        // A clone's id is always suffixed (the source already occupies the
        // unsuffixed one) — same convention as a repeat `asc install`
        // (DMN-033): the id doubles as the display name unless --name wins.
        custom_name: Some(
            custom_name
                .map(str::to_string)
                .unwrap_or_else(|| new_id.clone()),
        ),
        owner: Owner {
            uid: ctx.uid,
            name: ctx.name.clone(),
        },
        version: source.version.clone(),
        source: source.source.clone(),
        // Recorded like a suffixed install instance, so `asc app upgrade`
        // keeps resolving the clone against the same registry package.
        package: Some(source.package.clone().unwrap_or_else(|| source.id.clone())),
        desired_state: DesiredState::Stopped,
        quota,
        runtime,
    };
    store.save(&meta)?;
    cleanup.disarm();
    info!(app = %new_id, source = %source.id, "app cloned");
    Ok(meta)
}
