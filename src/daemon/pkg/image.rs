//! Image freshness and repull of a Docker app (DMN-120).
//!
//! [`status`] answers "which image does this app run, what version is it,
//! and is there a newer build of the same tag": the tag's local manifest
//! digest (`RepoDigests`) is compared with what the registry serves for it
//! now (`/distribution/{name}/json` — no layers downloaded), and the image
//! id the container was created from is compared with the id the tag points
//! at locally (a pull that has not been applied yet).
//!
//! [`repull`] refreshes a *mutable* tag (`latest`, or no tag at all) from
//! its registry. The container is not touched here: the settings-drift check
//! in [`super::refresh`] compares image ids, so the next start/restart
//! recreates the container onto the new image — the caller restarts a
//! running app right away. A pinned tag (`nginx:1.27`) or a digest is
//! updated by upgrading the package, not by re-pulling; a locally built
//! image has no registry to pull from.

use std::path::Path;

use anyhow::Result;

use super::dockerfile;
use super::install::{effective_image_ref, registry_auth_for};
use super::settings::locate_installed;
use crate::daemon::apps::meta::{AppMeta, Runtime};
use crate::daemon::config::Config;
use crate::daemon::docker;

/// OCI annotation most images carry their release version in.
const VERSION_LABEL: &str = "org.opencontainers.image.version";

/// Overall freshness verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageState {
    /// The local image is what the registry serves for the tag, and the
    /// container runs it.
    UpToDate,
    /// The registry serves a different image for the tag.
    UpdateAvailable,
    /// A newer image is already on the host but the container still runs
    /// the old one — a restart applies it.
    RestartRequired,
    /// Could not tell: a locally built image, an image not on the host, or
    /// the registry could not be asked.
    Unknown,
}

/// What [`status`] reports.
#[derive(Debug, Clone)]
pub struct ImageStatus {
    /// Effective image reference (`nginx:latest`, `asc-local/app:latest`).
    pub image: String,
    /// Tag part; `None` for a digest reference.
    pub tag: Option<String>,
    /// Built from the package's Dockerfile (DMN-050) rather than pulled.
    pub built_locally: bool,
    /// [`repull`] applies: pulled from a registry under a mutable tag.
    pub repullable: bool,
    /// `sha256:…` id the tag points at on the host; `None` when not pulled.
    pub image_id: Option<String>,
    /// `sha256:…` registry manifest digest of the local image.
    pub digest: Option<String>,
    /// `org.opencontainers.image.version`, else a non-`latest` tag.
    pub version: Option<String>,
    /// Unix seconds the image was built.
    pub created: Option<i64>,
    /// Id of the image the container was created from.
    pub running_image_id: Option<String>,
    /// What the registry serves for the tag right now.
    pub remote_digest: Option<String>,
    /// Why the registry could not be asked, when it could not.
    pub remote_error: Option<String>,
    pub state: ImageState,
}

/// What [`repull`] did.
#[derive(Debug, Clone)]
pub struct RepullOutcome {
    pub image: String,
    /// Id the container ran before the pull.
    pub previous_image_id: Option<String>,
    /// Id the tag points at after the pull.
    pub image_id: String,
    /// The container needs a recreate to run the pulled image.
    pub changed: bool,
}

/// Refusal of [`repull`]: nothing a pull could update. Rendered as
/// `FAILED_PRECONDITION` / REST 409.
#[derive(Debug)]
pub struct NotRepullable {
    pub app: String,
    pub reason: &'static str,
}

impl std::fmt::Display for NotRepullable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "app '{}' cannot be re-pulled: {}", self.app, self.reason)
    }
}

impl std::error::Error for NotRepullable {}

/// The image reference a Docker app runs and whether it is built locally.
fn resolve(config: &Config, meta: &AppMeta, app_dir: &Path) -> Result<Option<(String, bool)>> {
    let Runtime::Docker { image_source, .. } = &meta.runtime else {
        return Ok(None);
    };
    let (manifest_dir, _) = locate_installed(config, meta, app_dir)?;
    let (manifest, _) = dockerfile::resolve_installed(meta, &manifest_dir)?;
    let Some(image) = effective_image_ref(&manifest, *image_source, &meta.id) else {
        return Ok(None);
    };
    let built_locally = manifest.runtime.image.as_deref() != Some(image.as_str());
    Ok(Some((image, built_locally)))
}

/// A tag that moves on its own: `latest`, or none (which means `latest`).
fn mutable_tag(image: &str) -> bool {
    !image.contains('@') && docker::split_image_ref(image).1 == Some("latest")
}

/// The digest part of the `RepoDigests` entry for `image`'s repository,
/// falling back to the first entry (the Engine shortens Docker Hub names,
/// `docker.io/library/nginx` → `nginx`, so an exact match is not reliable).
fn local_digest(image: &str, repo_digests: &[String]) -> Option<String> {
    let (repository, _) = docker::split_image_ref(image);
    let short = repository
        .trim_start_matches("docker.io/")
        .trim_start_matches("library/");
    let digest_of = |entry: &String| entry.split_once('@').map(|(_, d)| d.to_string());
    repo_digests
        .iter()
        .find(|entry| {
            entry
                .split_once('@')
                .is_some_and(|(repo, _)| repo == repository || repo == short)
        })
        .or_else(|| repo_digests.first())
        .and_then(digest_of)
}

fn verdict(
    image_id: Option<&str>,
    running_image_id: Option<&str>,
    digest: Option<&str>,
    remote_digest: Option<&str>,
) -> ImageState {
    if let (Some(local), Some(running)) = (image_id, running_image_id)
        && local != running
    {
        return ImageState::RestartRequired;
    }
    match (digest, remote_digest) {
        (Some(local), Some(remote)) if local == remote => ImageState::UpToDate,
        (Some(_), Some(_)) => ImageState::UpdateAvailable,
        _ => ImageState::Unknown,
    }
}

/// Image freshness of a Docker app; `Ok(None)` for other runtimes. With
/// `check_remote` the registry is asked for the tag's current digest (a
/// network call, bounded); without it `state` only reflects the local side.
pub fn status(
    config: &Config,
    meta: &AppMeta,
    app_dir: &Path,
    check_remote: bool,
) -> Result<Option<ImageStatus>> {
    let Some((image, built_locally)) = resolve(config, meta, app_dir)? else {
        return Ok(None);
    };
    let Runtime::Docker { container, .. } = &meta.runtime else {
        return Ok(None);
    };
    let local = docker::inspect_local_image(&config.docker, &image)?;
    let running_image_id = docker::container_image_id(&config.docker, container)?;
    let (_, tag) = docker::split_image_ref(&image);
    let tag = tag.map(str::to_string);
    let digest = local
        .as_ref()
        .and_then(|l| local_digest(&image, &l.repo_digests));

    let (remote_digest, remote_error) = if check_remote && !built_locally {
        let auth = registry_auth_for(&image, &[Some(meta.id.as_str()), meta.uuid.as_deref()]);
        match docker::registry_digest(&config.docker, &image, auth.as_ref()) {
            Ok(digest) => (Some(digest), None),
            Err(err) => (None, Some(format!("{err:#}"))),
        }
    } else {
        (None, None)
    };

    let version = local
        .as_ref()
        .and_then(|l| l.labels.get(VERSION_LABEL).cloned())
        .filter(|v| !v.trim().is_empty())
        .or_else(|| tag.clone().filter(|t| t != "latest"));
    let image_id = local
        .as_ref()
        .map(|l| l.id.clone())
        .filter(|id| !id.is_empty());
    let state = verdict(
        image_id.as_deref(),
        running_image_id.as_deref(),
        digest.as_deref(),
        remote_digest.as_deref(),
    );
    Ok(Some(ImageStatus {
        repullable: !built_locally && mutable_tag(&image),
        built_locally,
        tag,
        created: local.as_ref().and_then(|l| l.created),
        image,
        image_id,
        digest,
        version,
        running_image_id,
        remote_digest,
        remote_error,
        state,
    }))
}

/// Pull a Docker app's mutable-tag image afresh. The container is left as
/// is — see the module docs for how the new image gets applied.
pub fn repull(config: &Config, meta: &AppMeta, app_dir: &Path) -> Result<RepullOutcome> {
    let not = |reason| NotRepullable {
        app: meta.id.clone(),
        reason,
    };
    let Runtime::Docker { container, .. } = &meta.runtime else {
        return Err(not("only Docker apps run an image").into());
    };
    let Some((image, built_locally)) = resolve(config, meta, app_dir)? else {
        return Err(not("the manifest names no image").into());
    };
    if built_locally {
        return Err(not("the image is built locally from the package's Dockerfile").into());
    }
    if !mutable_tag(&image) {
        return Err(not("the image tag is pinned — upgrade the app to change it").into());
    }
    let previous_image_id = docker::container_image_id(&config.docker, container)?;
    let auth = registry_auth_for(&image, &[Some(meta.id.as_str()), meta.uuid.as_deref()]);
    docker::pull_image(&config.docker, &image, auth.as_ref())?;
    let image_id = docker::inspect_local_image(&config.docker, &image)?
        .map(|l| l.id)
        .unwrap_or_default();
    Ok(RepullOutcome {
        changed: previous_image_id.as_deref() != Some(image_id.as_str()),
        image,
        previous_image_id,
        image_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_latest_is_mutable() {
        assert!(mutable_tag("nginx"));
        assert!(mutable_tag("nginx:latest"));
        assert!(mutable_tag("localhost:5000/team/app:latest"));
        assert!(!mutable_tag("localhost:5000/team/app:1.2"));
        assert!(!mutable_tag("nginx:1.27"));
        assert!(!mutable_tag("nginx@sha256:abc"));
    }

    #[test]
    fn local_digest_matches_short_docker_hub_names() {
        let digests = vec![
            "ghcr.io/other/thing@sha256:111".to_string(),
            "nginx@sha256:222".to_string(),
        ];
        assert_eq!(
            local_digest("nginx:latest", &digests).as_deref(),
            Some("sha256:222")
        );
        assert_eq!(
            local_digest("docker.io/library/nginx:latest", &digests).as_deref(),
            Some("sha256:222")
        );
        assert_eq!(
            local_digest("ghcr.io/other/thing", &digests).as_deref(),
            Some("sha256:111")
        );
        assert_eq!(local_digest("nginx", &[]), None);
    }

    /// Live check against a real Engine and Docker Hub: after a pull by tag
    /// the local `RepoDigests` digest equals what the registry serves — the
    /// comparison [`status`] relies on. Needs network; run with `--ignored`.
    #[test]
    #[ignore]
    fn live_local_digest_matches_registry() {
        let cfg = crate::daemon::config::DockerConfig::default();
        let image = "alpine:latest";
        docker::pull_image(&cfg, image, None).unwrap();
        let local = docker::inspect_local_image(&cfg, image).unwrap().unwrap();
        let remote = docker::registry_digest(&cfg, image, None).unwrap();
        assert_eq!(
            local_digest(image, &local.repo_digests).as_deref(),
            Some(remote.as_str())
        );
    }

    #[test]
    fn verdict_prefers_an_unapplied_pull() {
        assert_eq!(
            verdict(
                Some("sha256:new"),
                Some("sha256:old"),
                Some("d1"),
                Some("d1")
            ),
            ImageState::RestartRequired
        );
        assert_eq!(
            verdict(Some("i"), Some("i"), Some("d1"), Some("d1")),
            ImageState::UpToDate
        );
        assert_eq!(
            verdict(Some("i"), Some("i"), Some("d1"), Some("d2")),
            ImageState::UpdateAvailable
        );
        assert_eq!(
            verdict(Some("i"), Some("i"), Some("d1"), None),
            ImageState::Unknown
        );
        assert_eq!(verdict(None, None, None, None), ImageState::Unknown);
    }
}
