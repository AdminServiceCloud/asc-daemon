//! In-place update of a docker compose app: upgrade and rebuild.
//!
//! Every other runtime is upgraded by cloning the new version beside the
//! installed one and swapping `repository/`. A compose project cannot be:
//! it runs from inside `repository/`, so its relative bind mounts
//! (`./data:/data`) keep the app's data right there, and a swap would throw
//! that data away with the old copy. A compose app is therefore moved in
//! place — `git fetch` of one ref, then `git checkout` — which leaves every
//! untracked and ignored file where it is.
//!
//! The order is what keeps a failure harmless:
//!
//! 1. the app's operation lock is already held (the caller takes it);
//! 2. the starting point is recorded — commit, running state, and the image
//!    every container runs;
//! 3. the ref is fetched (objects only, the working tree is untouched);
//! 4. local edits of tracked files are saved as a patch, untracked files the
//!    new commit would overwrite are copied, both into `.asc/backups/`;
//! 5. the new commit is checked out — from here on any failure rolls back;
//! 6. the project is checked: its files exist, no file and no resolved
//!    `config` mounts a host path outside the app, the daemon's override
//!    file sheds entries of services that are gone, and `config -q` passes;
//! 7. images are pulled and built **while the old containers keep running**;
//! 8. only then are the containers force-recreated (`up --force-recreate`),
//!    keeping the stack's running/stopped state — the only downtime, and the
//!    one step a cancelled caller no longer interrupts;
//! 9. the new version is recorded.
//!
//! A failure in 3–7 puts back the old commit, the local edits, the copied
//! files, the override file and the old image tags; the old containers were
//! never touched. A failure in 8 does the same and recreates the previous
//! containers. Nothing here ever runs `down` or removes a volume.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use tracing::{info, warn};

use super::compose as pkg_compose;
use super::gitref::{self, FetchRef, head_commit, short_commit};
use super::install::{GitRemote, VersionSpec};
use super::rebuild::RebuildOutcome;
use super::upgrade::{
    UpgradeOutcome, UpgradeRef, already_at_tag, record_tracking, resolve_target, tracked_branch,
};
use crate::daemon::apps::compose::{ORPHANED_OVERRIDE_FILE, override_path, project_of};
use crate::daemon::apps::compose_override::{self, OverrideFile};
use crate::daemon::apps::meta::{AppMeta, Runtime};
use crate::daemon::apps::{AppManager, UserContext, layout};
use crate::daemon::compose::{self, ComposeProject};
use crate::daemon::config::Config;
use crate::daemon::docker;
use crate::daemon::progress::{self, InstallReporter, Uncancellable};

/// Directory under `.asc/` that keeps what an update had to overwrite.
const BACKUPS_DIR: &str = "backups";

/// What [`refresh`] did.
#[derive(Debug, Clone)]
pub(super) struct Refreshed {
    /// Commit before and after; equal when nothing was fetched or the ref
    /// had not moved.
    pub(super) from_commit: Option<String>,
    pub(super) to_commit: Option<String>,
    /// The ref name the fetch matched (the tag spelling that existed).
    pub(super) fetched: Option<String>,
    /// Every service of the project after the update.
    pub(super) services: Vec<String>,
    /// The stack was (and still is) running.
    pub(super) running: bool,
    /// `true` when the fetched commit was the installed one and the caller
    /// asked to stop there: nothing was changed at all.
    pub(super) unchanged: bool,
}

/// What to fetch before rebuilding, and whether an unchanged commit ends
/// the run early.
pub(super) struct Fetch {
    pub(super) remote: GitRemote,
    pub(super) target: FetchRef,
    /// Stop with [`Refreshed::unchanged`] when the fetched commit is the
    /// installed one.
    pub(super) stop_if_unchanged: bool,
}

/// `asc upgrade` of a compose app: resolve the ref exactly as for any other
/// runtime ([`resolve_target`]), then [`refresh`] in place. The caller holds
/// the operation lock.
pub(super) fn upgrade(
    config: &Config,
    ctx: &UserContext,
    manager: &AppManager,
    meta: AppMeta,
    requested_version: VersionSpec<'_>,
    target: &UpgradeRef,
    report: Option<&dyn InstallReporter>,
) -> Result<UpgradeOutcome> {
    let id = meta.id.clone();
    let git_url = git_source(&meta)?;
    let resolved = resolve_target(&meta, target, requested_version, &git_url, true, ctx)?;
    let moving_ref = resolved.checkout.is_none() || resolved.tracks_branch;
    if let Some(current) = already_at_tag(&meta, resolved.checkout.as_deref(), moving_ref) {
        record_tracking(manager, &meta, target)?;
        return Ok(UpgradeOutcome::UpToDate {
            id,
            version: current,
        });
    }
    let fetch_ref = match (&resolved.checkout, resolved.tracks_branch) {
        (None, _) => FetchRef::Head,
        (Some(branch), true) => FetchRef::Branch(branch.clone()),
        (Some(tag), false) => FetchRef::Tag(tag.clone()),
    };
    let new_branch = tracked_branch(&meta, target);
    // A switch of ref (a branch to a tag at the very same commit) is still
    // an upgrade: the app is rebuilt and starts reporting the new ref.
    let fetch = Fetch {
        remote: GitRemote::resolve(&git_url, ctx),
        target: fetch_ref,
        stop_if_unchanged: true,
    };
    let app_dir = manager.store().app_dir(&id)?;
    let outcome = refresh(config, &meta, &app_dir, Some(fetch), "upgrade", report)?;

    // The version an app reports is the ref it follows: the tag that was
    // fetched or the branch; nothing for the HEAD of an untagged repository.
    let new_version = match &resolved.checkout {
        Some(_) => outcome.fetched.clone(),
        None => None,
    };
    let ref_changed = new_version != meta.version || new_branch != meta.branch;
    if outcome.unchanged && !ref_changed {
        record_tracking(manager, &meta, target)?;
        return Ok(UpgradeOutcome::UpToDate {
            id,
            version: meta.version.clone().unwrap_or_else(|| {
                outcome
                    .to_commit
                    .as_deref()
                    .map(short_commit)
                    .unwrap_or_default()
            }),
        });
    }
    let outcome = if outcome.unchanged {
        // The same commit under another name: the app is rebuilt from what
        // it already has checked out and starts reporting the new ref.
        refresh(config, &meta, &app_dir, None, "upgrade", report)?
    } else {
        outcome
    };
    save(manager, &id, |saved| {
        saved.version = new_version.clone();
        saved.branch = new_branch.clone();
    })?;
    let to = new_version.clone().unwrap_or_else(|| {
        outcome
            .to_commit
            .as_deref()
            .map(short_commit)
            .unwrap_or_default()
    });
    info!(
        app = %id,
        from = %meta.version.as_deref().unwrap_or("-"),
        to = %to,
        from_commit = %outcome.from_commit.as_deref().unwrap_or("-"),
        to_commit = %outcome.to_commit.as_deref().unwrap_or("-"),
        services = %outcome.services.join(","),
        running = outcome.running,
        "compose app upgraded"
    );
    Ok(UpgradeOutcome::Upgraded {
        id,
        from: meta.version.clone(),
        to,
        from_commit: outcome.from_commit,
        to_commit: outcome.to_commit,
    })
}

/// `asc rebuild` of a compose app: the whole project (with `pull_source`,
/// the ref it already follows is fetched first — the version never
/// changes), or one `service` alone. The caller holds the operation lock.
pub(super) fn rebuild(
    config: &Config,
    ctx: &UserContext,
    meta: AppMeta,
    app_dir: &Path,
    service: Option<&str>,
    pull_source: bool,
    report: Option<&dyn InstallReporter>,
) -> Result<RebuildOutcome> {
    if let Some(service) = service {
        return rebuild_service(config, &meta, app_dir, service, report);
    }
    let fetch = if pull_source {
        Some(Fetch {
            remote: GitRemote::resolve(&git_source(&meta)?, ctx),
            target: current_ref(&meta),
            stop_if_unchanged: false,
        })
    } else {
        None
    };
    let outcome = refresh(config, &meta, app_dir, fetch, "rebuild", report)?;
    Ok(RebuildOutcome {
        id: meta.id,
        source_updated: outcome.from_commit != outcome.to_commit,
        services: outcome.services,
        from_commit: outcome.from_commit,
        to_commit: outcome.to_commit,
        running: outcome.running,
    })
}

/// One service rebuilt alone: `build --pull <service>` (it has a `build:`
/// section) or `pull <service>`, then `up --no-deps --force-recreate
/// <service>` — `-d` when it runs, `--no-start` when it does not. The other
/// services are not touched. A failure points the service's image tag back
/// at the image its container runs and, when the recreate itself failed,
/// recreates it on that image again.
fn rebuild_service(
    config: &Config,
    meta: &AppMeta,
    app_dir: &Path,
    service: &str,
    report: Option<&dyn InstallReporter>,
) -> Result<RebuildOutcome> {
    let cfg = &config.docker;
    if !compose::available(cfg) {
        bail!("the docker compose plugin is not installed on this host");
    }
    let Runtime::Compose {
        project: project_name,
        ..
    } = &meta.runtime
    else {
        bail!("app '{}' is not a compose app", meta.id);
    };
    let project = project_of(meta, app_dir)?;
    let resolved = compose::config_resolved(cfg, &project)?;
    let Some(definition) = resolved.services.get(service) else {
        return Err(crate::daemon::exec::not_found(crate::daemon::i18n::tf2(
            crate::daemon::i18n::Msg::AppServiceNotFound,
            service,
            &meta.id,
        )));
    };
    pkg_compose::check_resolved_mounts(&resolved, app_dir)?;
    let containers = docker::list_project_containers(cfg, project_name)?;
    let mine: Vec<&docker::ContainerInfo> = containers
        .iter()
        .filter(|c| c.compose_service.as_deref() == Some(service))
        .collect();
    let running = mine.iter().any(|c| c.state == "running");
    let images = image_snapshot(cfg, mine.iter().copied());
    let names = [service.to_string()];
    let mut recreate_attempted = false;
    let result = (|| -> Result<()> {
        if definition.buildable() {
            compose::build(cfg, &project, &names, true, report)?;
        } else if definition.image.is_some() {
            compose::pull(cfg, &project, &names, report)?;
        }
        progress::ensure_not_cancelled(report)?;
        recreate_attempted = true;
        compose::up_recreate(
            cfg,
            &project,
            running,
            Some(service),
            Some(&Uncancellable(report)),
        )
    })();
    if let Err(err) = result {
        for (reference, id) in &images {
            if let Err(err) = docker::tag_image(cfg, id, reference) {
                warn!(app = %meta.id, service, error = %format!("{err:#}"), "rollback: cannot restore an image tag");
            }
        }
        if recreate_attempted
            && let Err(err) = compose::up_recreate(
                cfg,
                &project,
                running,
                Some(service),
                Some(&Uncancellable(report)),
            )
        {
            warn!(app = %meta.id, service, error = %format!("{err:#}"), "rollback: cannot recreate the service");
        }
        return Err(err.context(format!(
            "rebuild of service '{service}' of '{}' failed, previous image restored",
            meta.id
        )));
    }
    let commit = head_commit(&app_dir.join("repository"));
    Ok(RebuildOutcome {
        id: meta.id.clone(),
        services: names.to_vec(),
        from_commit: commit.clone(),
        to_commit: commit,
        source_updated: false,
        running,
    })
}

/// The ref a compose app already follows: its branch, the tag it is pinned
/// to, or the default branch HEAD — what a rebuild with `pull_source`
/// fetches, never moving the app to another ref.
pub(super) fn current_ref(meta: &AppMeta) -> FetchRef {
    match (&meta.branch, &meta.version) {
        (Some(branch), _) => FetchRef::Branch(branch.clone()),
        (None, Some(tag)) => FetchRef::Tag(tag.clone()),
        (None, None) => FetchRef::Head,
    }
}

/// The recorded repository URL of a compose app — always a direct git
/// install (`git:<url>`); a registry entry is never a compose project.
pub(super) fn git_source(meta: &AppMeta) -> Result<String> {
    meta.source
        .as_deref()
        .and_then(|s| s.strip_prefix("git:"))
        .map(str::to_string)
        .ok_or_else(|| anyhow!("app '{}' has no git source to fetch from", meta.id))
}

/// Reload the app's meta, apply `change` and save it: the run may have
/// taken minutes, and fields it does not own (a rename) must not be
/// clobbered with what was loaded at its start.
pub(super) fn save(
    manager: &AppManager,
    id: &str,
    change: impl FnOnce(&mut AppMeta),
) -> Result<()> {
    let mut meta = manager
        .store()
        .get(id)?
        .ok_or_else(|| anyhow!("app '{id}' disappeared during the update"))?;
    change(&mut meta);
    manager.store().save(&meta)
}

/// Steps 2–9 of the module docs over the whole project: fetch (optional),
/// protect local edits, check out, check, pull/build, recreate, record the
/// project's files and services. `operation` (`upgrade`/`rebuild`) names
/// the backups and the error. The caller records the version and holds the
/// operation lock.
pub(super) fn refresh(
    config: &Config,
    meta: &AppMeta,
    app_dir: &Path,
    fetch: Option<Fetch>,
    operation: &str,
    report: Option<&dyn InstallReporter>,
) -> Result<Refreshed> {
    let docker_cfg = &config.docker;
    if !compose::available(docker_cfg) {
        bail!("the docker compose plugin is not installed on this host");
    }
    let Runtime::Compose {
        project: project_name,
        files,
        working_dir,
        ..
    } = &meta.runtime
    else {
        bail!("app '{}' is not a compose app", meta.id);
    };
    let repo = app_dir.join("repository");
    let old_project = project_of(meta, app_dir)?;
    if let Some(report) = report {
        report.line(&format!(
            "docker compose {}",
            compose::version(docker_cfg)
                .as_deref()
                .unwrap_or("(unknown version)")
        ));
    }

    // 2. Where everything stands now.
    let from_commit = head_commit(&repo);
    let containers = docker::list_project_containers(docker_cfg, project_name)?;
    let was_running = containers.iter().any(|c| c.state == "running");
    let images = image_snapshot(docker_cfg, &containers);
    let mut rollback = Rollback {
        repo: repo.clone(),
        from_commit: from_commit.clone(),
        checked_out: false,
        patch: None,
        copied: Vec::new(),
        images,
        images_touched: false,
        override_path: override_path(app_dir),
        override_before: None,
        orphaned_before: None,
        recreate_attempted: false,
        was_running,
        old_project,
    };

    let result = (|| -> Result<Refreshed> {
        // 3. Fetch: objects only, nothing in the working tree changes yet.
        let mut to_commit = from_commit.clone();
        let mut fetched = None;
        if let Some(fetch) = &fetch {
            let Some(from) = &from_commit else {
                bail!(
                    "{} is not a git repository — cannot update it in place",
                    repo.display()
                );
            };
            let (name, commit) = gitref::fetch_ref(&repo, &fetch.remote, &fetch.target, report)?;
            fetched = Some(name);
            if &commit == from && fetch.stop_if_unchanged {
                return Ok(Refreshed {
                    from_commit: from_commit.clone(),
                    to_commit: Some(commit),
                    fetched,
                    services: Vec::new(),
                    running: was_running,
                    unchanged: true,
                });
            }
            if &commit != from {
                progress::ensure_not_cancelled(report)?;
                // 4. Whatever the checkout would overwrite is kept first.
                protect_local_changes(&mut rollback, app_dir, from, &commit, operation, report)?;
                // 5. From here on, any failure rolls back.
                if let Some(report) = report {
                    report.line(&format!(
                        "Checking out {} ({})",
                        short_commit(&commit),
                        fetch_label(&fetch.target)
                    ));
                }
                rollback.checked_out = true;
                gitref::checkout_detached(&repo, &commit)?;
            }
            to_commit = Some(commit);
        }
        progress::ensure_not_cancelled(report)?;

        // 6. The project the new commit describes, checked.
        let working = app_dir.join(working_dir);
        let files = current_files(&working, files, report)?;
        pkg_compose::check_bind_mounts_all(&working, &files, &working)?;
        let base = ComposeProject::new(project_name.clone(), &working, files.clone());
        let services = compose::config_services(docker_cfg, &base)?;
        let base_resolved = compose::config_resolved(docker_cfg, &base)?;
        prune_override(&mut rollback, app_dir, &services, &base_resolved, report)?;
        let project = if rollback.override_path.is_file() {
            base.with_file(&rollback.override_path)
        } else {
            base
        };
        let resolved = compose::config_resolved(docker_cfg, &project)?;
        pkg_compose::check_resolved_mounts(&resolved, app_dir)?;
        compose::config_validate(docker_cfg, &project)?;
        progress::ensure_not_cancelled(report)?;

        // 7. Images, while the old containers keep running.
        let (build, pull) = split_build_pull(&resolved, &services);
        rollback.images_touched = true;
        compose::pull(docker_cfg, &project, &pull, report)?;
        compose::build(docker_cfg, &project, &build, true, report)?;
        progress::ensure_not_cancelled(report)?;

        // 8. The one moment the stack goes down — never interrupted.
        rollback.recreate_attempted = true;
        compose::up_recreate(
            docker_cfg,
            &project,
            was_running,
            None,
            Some(&Uncancellable(report)),
        )?;

        // 9. The project's shape as the new commit has it.
        record_project(meta, app_dir, &files, &services)?;
        Ok(Refreshed {
            from_commit: from_commit.clone(),
            to_commit,
            fetched,
            services,
            running: was_running,
            unchanged: false,
        })
    })();

    match result {
        Ok(refreshed) => Ok(refreshed),
        Err(err) => {
            if let Some(report) = report {
                report.line(&format!("{operation} failed: {err:#}"));
                report.line("restoring the previous version");
            }
            rollback.run(config, report);
            Err(err.context(format!(
                "{operation} of '{}' failed, previous version restored",
                meta.id
            )))
        }
    }
}

/// `meta.files`, or — when the new commit moved the compose file — the one
/// a fresh install would pick now.
fn current_files(
    working: &Path,
    files: &[String],
    report: Option<&dyn InstallReporter>,
) -> Result<Vec<String>> {
    if !working.is_dir() {
        bail!(
            "the compose project directory {} no longer exists in this version",
            working.display()
        );
    }
    if files.iter().all(|file| working.join(file).is_file()) {
        return Ok(files.to_vec());
    }
    let found = pkg_compose::find(working)?;
    if let Some(report) = report {
        report.line(&format!(
            "compose file {} is gone, using {found}",
            files.join(", ")
        ));
    }
    Ok(vec![found])
}

/// Save the project's files and services into the app's meta (version and
/// branch are the caller's).
fn record_project(
    meta: &AppMeta,
    app_dir: &Path,
    files: &[String],
    services: &[String],
) -> Result<()> {
    let mut saved = AppMeta::load(app_dir).unwrap_or_else(|_| meta.clone());
    if let Runtime::Compose {
        files: saved_files,
        services: saved_services,
        ..
    } = &mut saved.runtime
    {
        *saved_files = files.to_vec();
        *saved_services = services.to_vec();
    }
    saved.save(app_dir)
}

/// The `ref → id` of every image the project's containers run, before
/// anything is pulled or built — what a rollback points the tags back at.
fn image_snapshot<'a>(
    cfg: &crate::daemon::config::DockerConfig,
    containers: impl IntoIterator<Item = &'a docker::ContainerInfo>,
) -> Vec<(String, String)> {
    let mut images: Vec<(String, String)> = Vec::new();
    for container in containers {
        match docker::container_image(cfg, &container.id) {
            Ok(Some((reference, id))) => {
                if !reference.starts_with("sha256:") && !images.iter().any(|(r, _)| *r == reference)
                {
                    images.push((reference, id));
                }
            }
            Ok(None) => {}
            Err(err) => {
                warn!(container = %container.id, error = %format!("{err:#}"), "cannot read the image of a compose container");
            }
        }
    }
    images
}

/// Step 4: a patch of the local edits of tracked files and copies of the
/// untracked files `to` would overwrite, under `.asc/backups/`, with one
/// warning line naming them. The checkout then overwrites both.
fn protect_local_changes(
    rollback: &mut Rollback,
    app_dir: &Path,
    from: &str,
    to: &str,
    operation: &str,
    report: Option<&dyn InstallReporter>,
) -> Result<()> {
    let repo = &rollback.repo;
    let edited = gitref::tracked_changes(repo)?;
    let in_the_way = gitref::untracked_in_the_way(repo, to)?;
    if edited.is_empty() && in_the_way.is_empty() {
        return Ok(());
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let backups = layout::ensure_state_dir(app_dir)?.join(BACKUPS_DIR);
    fs::create_dir_all(&backups).with_context(|| format!("cannot create {}", backups.display()))?;
    let name = format!("{operation}-{}-{stamp}", short_commit(from));
    let mut saved = Vec::new();
    if !edited.is_empty() {
        let patch = backups.join(format!("{name}.patch"));
        fs::write(&patch, gitref::tracked_changes_patch(repo)?)
            .with_context(|| format!("cannot write {}", patch.display()))?;
        saved.push(patch.display().to_string());
        rollback.patch = Some(patch);
    }
    if !in_the_way.is_empty() {
        let copies = backups.join(&name);
        for rel in &in_the_way {
            let source = repo.join(rel);
            if !source.is_file() {
                continue;
            }
            let copy = copies.join(rel);
            if let Some(parent) = copy.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("cannot create {}", parent.display()))?;
            }
            fs::copy(&source, &copy)
                .with_context(|| format!("cannot copy {}", source.display()))?;
            rollback.copied.push((copy, source));
        }
        saved.push(format!("{}/", copies.display()));
    }
    if let Some(report) = report {
        let mut files: Vec<&str> = edited.iter().map(String::as_str).collect();
        files.extend(in_the_way.iter().map(String::as_str));
        report.line(&format!(
            "warning: local changes will be overwritten by the new version: {} — saved to {}",
            files.join(", "),
            saved.join(", ")
        ));
    }
    warn!(app_dir = %app_dir.display(), edited = edited.len(), untracked = in_the_way.len(),
        "local changes saved before the in-place update");
    Ok(())
}

/// Step 6, the override file: entries of services the new version no
/// longer has move to `.asc/compose.override.orphaned.yaml` (with a warning)
/// instead of failing the whole project; what is left is rewritten in the
/// limit keys the new files use.
fn prune_override(
    rollback: &mut Rollback,
    app_dir: &Path,
    services: &[String],
    base: &compose::ResolvedProject,
    report: Option<&dyn InstallReporter>,
) -> Result<()> {
    let path = rollback.override_path.clone();
    let before = match fs::read(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    let mut file = OverrideFile::parse(&String::from_utf8_lossy(&before))
        .with_context(|| format!("invalid {}", path.display()))?;
    let orphaned = file.prune(services);
    rollback.override_before = Some(before);
    if !orphaned.services.is_empty() {
        let orphaned_path = layout::state_dir(app_dir).join(ORPHANED_OVERRIDE_FILE);
        rollback.orphaned_before = Some(fs::read(&orphaned_path).ok());
        let mut kept = OverrideFile::load(&orphaned_path).unwrap_or_default();
        kept.services.extend(orphaned.services.clone());
        compose_override::store(&kept, &compose::ResolvedProject::default(), &orphaned_path)?;
        if let Some(report) = report {
            report.line(&format!(
                "warning: settings of services that no longer exist ({}) moved to {}",
                orphaned
                    .services
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", "),
                orphaned_path.display()
            ));
        }
    }
    compose_override::store(&file, base, &path)
}

fn fetch_label(target: &FetchRef) -> String {
    match target {
        FetchRef::Branch(branch) => format!("branch {branch}"),
        FetchRef::Tag(tag) => format!("tag {tag}"),
        FetchRef::Head => "default branch".to_string(),
    }
}

/// Everything a failed update has to put back, filled in as it goes.
struct Rollback {
    repo: PathBuf,
    from_commit: Option<String>,
    checked_out: bool,
    patch: Option<PathBuf>,
    /// `(backup copy, original path)`.
    copied: Vec<(PathBuf, PathBuf)>,
    /// `(image ref, image id)` the old containers run.
    images: Vec<(String, String)>,
    images_touched: bool,
    override_path: PathBuf,
    /// The override file as it was, when the update rewrote it.
    override_before: Option<Vec<u8>>,
    /// The orphaned-settings file as it was (`None` inside: did not exist).
    orphaned_before: Option<Option<Vec<u8>>>,
    recreate_attempted: bool,
    was_running: bool,
    /// The project as it was — files, override and all.
    old_project: ComposeProject,
}

impl Rollback {
    /// Best effort, step by step: one thing failing to come back must not
    /// stop the others from being restored. Never cancellable.
    fn run(&self, config: &Config, report: Option<&dyn InstallReporter>) {
        let note = |what: &str, err: &anyhow::Error| {
            warn!(repo = %self.repo.display(), error = %format!("{err:#}"), "rollback: {what}");
            if let Some(report) = report {
                report.line(&format!("rollback: {what}: {err:#}"));
            }
        };
        if let Some(before) = &self.override_before
            && let Err(err) = fs::write(&self.override_path, before)
                .with_context(|| format!("cannot write {}", self.override_path.display()))
        {
            note("cannot restore the override file", &err);
        }
        if let Some(before) = &self.orphaned_before {
            let path = self.override_path.with_file_name(ORPHANED_OVERRIDE_FILE);
            let restored = match before {
                Some(raw) => fs::write(&path, raw),
                None => fs::remove_file(&path),
            };
            if let Err(err) = restored {
                note("cannot restore the orphaned settings file", &err.into());
            }
        }
        if self.checked_out
            && let Some(from) = &self.from_commit
        {
            match gitref::checkout_detached(&self.repo, from) {
                Ok(()) => {
                    if let Some(patch) = &self.patch
                        && let Err(err) = gitref::apply_patch(&self.repo, patch)
                    {
                        note("cannot re-apply the local changes", &err);
                    }
                    for (copy, original) in &self.copied {
                        if let Err(err) = fs::copy(copy, original) {
                            note("cannot restore an overwritten file", &err.into());
                        }
                    }
                }
                Err(err) => note("cannot check the previous commit out again", &err),
            }
        }
        if self.images_touched {
            for (reference, id) in &self.images {
                if let Err(err) = docker::tag_image(&config.docker, id, reference) {
                    note(
                        &format!("cannot point {reference} back at its old image"),
                        &err,
                    );
                }
            }
        }
        if self.recreate_attempted
            && let Err(err) = compose::up_recreate(
                &config.docker,
                &self.old_project,
                self.was_running,
                None,
                Some(&Uncancellable(report)),
            )
        {
            note("cannot recreate the previous containers", &err);
        }
    }
}

/// Split the project's `services` into `(built, pulled)` names: a service
/// with a `build:` section is rebuilt from source, any other one with an
/// image is pulled.
pub(super) fn split_build_pull(
    resolved: &compose::ResolvedProject,
    services: &[String],
) -> (Vec<String>, Vec<String>) {
    let mut build = Vec::new();
    let mut pull = Vec::new();
    for (name, service) in &resolved.services {
        if !services.iter().any(|s| s == name) {
            continue;
        }
        if service.buildable() {
            build.push(name.clone());
        } else if service.image.is_some() {
            pull.push(name.clone());
        }
    }
    (build, pull)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::apps::meta::{DesiredState, Owner};

    fn meta(branch: Option<&str>, version: Option<&str>) -> AppMeta {
        AppMeta {
            id: "demo".into(),
            uuid: None,
            name: "demo".into(),
            custom_name: None,
            owner: Owner {
                uid: 0,
                name: "root".into(),
            },
            version: version.map(str::to_string),
            source: Some("git:https://example.com/demo".into()),
            branch: branch.map(str::to_string),
            repo_path: None,
            package: None,
            install_method: None,
            desired_state: DesiredState::Stopped,
            quota: None,
            runtime: Runtime::Compose {
                project: "asc-demo".into(),
                files: vec!["docker-compose.yml".into()],
                working_dir: "repository".into(),
                services: Vec::new(),
            },
        }
    }

    #[test]
    fn a_rebuild_fetches_the_ref_the_app_already_follows() {
        assert_eq!(
            current_ref(&meta(Some("main"), Some("main"))),
            FetchRef::Branch("main".into())
        );
        assert_eq!(
            current_ref(&meta(None, Some("v1.2.0"))),
            FetchRef::Tag("v1.2.0".into())
        );
        assert_eq!(current_ref(&meta(None, None)), FetchRef::Head);
    }

    #[test]
    fn the_git_source_comes_from_a_direct_install_only() {
        assert_eq!(
            git_source(&meta(None, None)).unwrap(),
            "https://example.com/demo"
        );
        let mut registry = meta(None, None);
        registry.source = Some("official:https://example.com/demo".into());
        assert!(git_source(&registry).is_err());
    }

    #[test]
    fn built_services_are_built_and_the_rest_pulled() {
        let resolved: compose::ResolvedProject = serde_yaml::from_str(
            "services:\n  web:\n    build:\n      context: .\n    image: demo-web\n  db:\n    image: postgres:16\n  sidecar:\n    command: [\"true\"]\n",
        )
        .unwrap();
        let all = ["web".to_string(), "db".to_string(), "sidecar".to_string()];
        let (build, pull) = split_build_pull(&resolved, &all);
        assert_eq!(build, ["web"]);
        assert_eq!(pull, ["db"]);
        // Only the services asked for.
        let (build, pull) = split_build_pull(&resolved, &["db".to_string()]);
        assert!(build.is_empty());
        assert_eq!(pull, ["db"]);
    }

    /// A throwaway repository with one commit, as an installed app's
    /// `repository/` looks after a clone.
    fn repo_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q"]);
        for (path, content) in files {
            let path = dir.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "init"]);
        dir
    }

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok()
    }

    #[test]
    fn local_changes_are_saved_and_come_back_on_rollback() {
        if !git_available() {
            eprintln!("skipping: git is not available");
            return;
        }
        let repo = repo_with(&[("app.conf", "v1\n"), (".gitignore", "secrets.env\n")]);
        let from = head_commit(repo.path()).unwrap();
        // A second commit that starts tracking a file the user created
        // locally — the "untracked file in the way" case.
        fs::write(repo.path().join("app.conf"), "v2\n").unwrap();
        fs::write(repo.path().join("secrets.env"), "upstream default\n").unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(args)
                .current_dir(repo.path())
                .output()
                .unwrap()
        };
        git(&["add", "-f", "app.conf", "secrets.env"]);
        git(&["commit", "-q", "-m", "v2"]);
        let to = head_commit(repo.path()).unwrap();
        // Back on v1 with the user's own edits on top.
        gitref::checkout_detached(repo.path(), &from).unwrap();
        fs::write(repo.path().join("app.conf"), "v1 + local edit\n").unwrap();
        fs::write(repo.path().join("secrets.env"), "my password\n").unwrap();

        let app = tempfile::tempdir().unwrap();
        let mut rollback = Rollback {
            repo: repo.path().to_path_buf(),
            from_commit: Some(from.clone()),
            checked_out: false,
            patch: None,
            copied: Vec::new(),
            images: Vec::new(),
            images_touched: false,
            override_path: override_path(app.path()),
            override_before: None,
            orphaned_before: None,
            recreate_attempted: false,
            was_running: false,
            old_project: ComposeProject::new("asc-demo", repo.path(), vec![]),
        };
        protect_local_changes(&mut rollback, app.path(), &from, &to, "upgrade", None).unwrap();
        let patch = rollback.patch.clone().expect("a patch of the tracked edit");
        assert!(patch.starts_with(app.path().join(".asc/backups")));
        assert!(fs::read_to_string(&patch).unwrap().contains("local edit"));
        assert_eq!(rollback.copied.len(), 1);
        assert_eq!(
            fs::read_to_string(&rollback.copied[0].0).unwrap(),
            "my password\n"
        );

        // The update goes ahead and overwrites both...
        rollback.checked_out = true;
        gitref::checkout_detached(repo.path(), &to).unwrap();
        assert_eq!(
            fs::read_to_string(repo.path().join("secrets.env")).unwrap(),
            "upstream default\n"
        );
        // ...and a failure puts everything back as the user left it.
        rollback.run(&Config::default(), None);
        assert_eq!(head_commit(repo.path()).unwrap(), from);
        assert_eq!(
            fs::read_to_string(repo.path().join("app.conf")).unwrap(),
            "v1 + local edit\n"
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("secrets.env")).unwrap(),
            "my password\n"
        );
    }

    #[test]
    fn nothing_is_saved_without_local_changes() {
        if !git_available() {
            eprintln!("skipping: git is not available");
            return;
        }
        let repo = repo_with(&[("app.conf", "v1\n")]);
        let from = head_commit(repo.path()).unwrap();
        let app = tempfile::tempdir().unwrap();
        let mut rollback = Rollback {
            repo: repo.path().to_path_buf(),
            from_commit: Some(from.clone()),
            checked_out: false,
            patch: None,
            copied: Vec::new(),
            images: Vec::new(),
            images_touched: false,
            override_path: override_path(app.path()),
            override_before: None,
            orphaned_before: None,
            recreate_attempted: false,
            was_running: false,
            old_project: ComposeProject::new("asc-demo", repo.path(), vec![]),
        };
        protect_local_changes(&mut rollback, app.path(), &from, &from, "rebuild", None).unwrap();
        assert!(rollback.patch.is_none());
        assert!(rollback.copied.is_empty());
        assert!(!app.path().join(".asc/backups").exists());
    }

    #[test]
    fn pruned_settings_are_set_aside_and_restored_on_rollback() {
        let app = tempfile::tempdir().unwrap();
        let path = override_path(app.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original =
            "services:\n  web:\n    restart: always\n  gone:\n    environment:\n      A: \"1\"\n";
        fs::write(&path, original).unwrap();
        let mut rollback = Rollback {
            repo: app.path().join("repository"),
            from_commit: None,
            checked_out: false,
            patch: None,
            copied: Vec::new(),
            images: Vec::new(),
            images_touched: false,
            override_path: path.clone(),
            override_before: None,
            orphaned_before: None,
            recreate_attempted: false,
            was_running: false,
            old_project: ComposeProject::new("asc-demo", app.path(), vec![]),
        };
        prune_override(
            &mut rollback,
            app.path(),
            &["web".into()],
            &compose::ResolvedProject::default(),
            None,
        )
        .unwrap();
        let kept = OverrideFile::load(&path).unwrap();
        assert_eq!(kept.services.keys().collect::<Vec<_>>(), ["web"]);
        let orphaned_path = app.path().join(".asc").join(ORPHANED_OVERRIDE_FILE);
        let orphaned = OverrideFile::load(&orphaned_path).unwrap();
        assert_eq!(orphaned.services.keys().collect::<Vec<_>>(), ["gone"]);

        rollback.run(&Config::default(), None);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert!(!orphaned_path.exists());
    }
}
