//! Install flow: resolve package → clone repository → read manifest →
//! provision runtime → write meta.json.
//!
//! Installing is atomic from the user's point of view: any failure removes
//! the half-created app directory, so a retry starts clean.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use super::manifest::{AppType, Manifest};
use super::registry::RegistryClient;
use crate::daemon::apps::meta::{AppMeta, DesiredState, Owner, Runtime};
use crate::daemon::apps::{AppStore, UserContext};
use crate::daemon::config::{Config, DockerConfig};
use crate::daemon::docker;
use crate::daemon::i18n::{Msg, t, tf};

#[derive(Debug)]
pub struct InstallReport {
    pub id: String,
    pub version: String,
}

/// Remove a directory unless `disarm` was called — cleanup for failed installs.
struct RemoveOnDrop {
    path: PathBuf,
    armed: bool,
}

impl RemoveOnDrop {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if self.armed
            && let Err(err) = fs::remove_dir_all(&self.path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            warn!(dir = %self.path.display(), error = %err, "cannot clean up failed install");
        }
    }
}

/// Install `name` or `name@version` from the configured registries.
pub fn install(config: &Config, ctx: &UserContext, spec: &str) -> Result<InstallReport> {
    let (name, requested_version) = parse_spec(spec);
    let store = AppStore::new(config.daemon.apps_dir.clone());
    if store.get(name)?.is_some() {
        bail!(tf(Msg::PkgAlreadyInstalled, name));
    }

    let resolved = RegistryClient::new(config)?.resolve(name)?;
    let version = requested_version
        .map(str::to_string)
        .or_else(|| resolved.entry.latest.clone());

    let app_dir = store.app_dir(name)?;
    fs::create_dir_all(&app_dir)
        .with_context(|| format!("cannot create app directory {}", app_dir.display()))?;
    let mut cleanup = RemoveOnDrop {
        path: app_dir.clone(),
        armed: true,
    };

    let repo_dir = app_dir.join("repository");
    let cloned_tag = clone_repository(&resolved.entry.source.git, version.as_deref(), &repo_dir)?;

    let manifest_dir = manifest_dir(&repo_dir, resolved.entry.source.path.as_deref())?;
    let manifest = Manifest::load(&manifest_dir)?;

    // Root policy (DMN-003): regular users may be limited to Docker apps.
    // The app type is only known after reading the manifest; the cleanup
    // guard removes the cloned repository on this failure path too.
    if !ctx.is_root
        && config.policy.user_install == crate::daemon::config::UserInstall::Docker
        && manifest.app_type != AppType::Docker
    {
        bail!(tf(Msg::PkgPolicyDockerOnly, name));
    }

    for sub in ["config", "data"] {
        fs::create_dir_all(app_dir.join(sub))
            .with_context(|| format!("cannot create {sub}/ in app directory"))?;
    }

    let runtime = provision(&manifest, name, &app_dir, &manifest_dir, &config.docker)?;

    let effective_version = cloned_tag.unwrap_or_else(|| manifest.version.clone());
    let meta = AppMeta {
        id: name.to_string(),
        name: manifest.title.clone().unwrap_or_else(|| name.to_string()),
        owner: Owner {
            uid: ctx.uid,
            name: ctx.name.clone(),
        },
        version: Some(effective_version.clone()),
        source: Some(format!(
            "{}:{}",
            resolved.source_name, resolved.entry.source.git
        )),
        desired_state: DesiredState::Stopped,
        runtime,
    };
    store.save(&meta)?;
    cleanup.disarm();
    info!(app = name, version = %effective_version, "app installed");
    Ok(InstallReport {
        id: name.to_string(),
        version: effective_version,
    })
}

/// `name@1.2.0` → (`name`, Some(`1.2.0`)).
fn parse_spec(spec: &str) -> (&str, Option<&str>) {
    match spec.split_once('@') {
        Some((name, version)) if !version.is_empty() => (name, Some(version)),
        _ => (spec, None),
    }
}

/// Clone the package repository; versions are git tags. Returns the tag that
/// was actually checked out (tries `1.2.0`, then `v1.2.0`), or `None` when
/// no version was requested and the default branch HEAD was cloned.
fn clone_repository(git_url: &str, version: Option<&str>, dest: &Path) -> Result<Option<String>> {
    match version {
        Some(tag) => {
            let candidates = [tag.to_string(), format!("v{tag}")];
            let mut last_err = String::new();
            for candidate in &candidates {
                match git_clone(git_url, Some(candidate), dest) {
                    Ok(()) => return Ok(Some(candidate.clone())),
                    Err(err) => {
                        last_err = format!("{err:#}");
                        // A failed clone may leave a partial directory behind.
                        let _ = fs::remove_dir_all(dest);
                    }
                }
            }
            bail!("cannot clone {git_url} at tag '{tag}' (also tried 'v{tag}'): {last_err}")
        }
        None => {
            git_clone(git_url, None, dest)?;
            Ok(None)
        }
    }
}

fn git_clone(git_url: &str, tag: Option<&str>, dest: &Path) -> Result<()> {
    let mut args: Vec<&str> = vec!["clone", "--depth", "1"];
    if let Some(tag) = tag {
        args.extend(["--branch", tag]);
    }
    args.push(git_url);
    let dest_str = dest.to_string_lossy();
    args.push(&dest_str);

    let out = match Command::new("git").args(&args).output() {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(t(Msg::ErrGitNotFound)),
        Err(e) => return Err(e).context("cannot run git"),
    };
    if !out.status.success() {
        bail!(
            "git clone failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Manifest directory inside the repository (monorepo packages set `path`).
fn manifest_dir(repo_dir: &Path, sub: Option<&str>) -> Result<PathBuf> {
    match sub {
        None => Ok(repo_dir.to_path_buf()),
        Some(sub) => {
            // The path comes from a registry file — never let it escape the repo.
            // `has_root` also catches "/abs", which is not `is_absolute` on Windows.
            let clean = Path::new(sub);
            if clean.is_absolute()
                || clean.has_root()
                || clean
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                bail!("invalid package path '{sub}' in registry entry");
            }
            Ok(repo_dir.join(clean))
        }
    }
}

/// Prepare the runtime described by the manifest and return its meta form.
fn provision(
    manifest: &Manifest,
    id: &str,
    app_dir: &Path,
    manifest_dir: &Path,
    docker_cfg: &DockerConfig,
) -> Result<Runtime> {
    match manifest.app_type {
        AppType::Docker => {
            let image = manifest
                .runtime
                .image
                .as_deref()
                .expect("validated: docker manifests have an image");
            let container = format!("asc-{id}");
            docker_create(manifest, image, &container, app_dir, docker_cfg)?;
            Ok(Runtime::Docker { container })
        }
        AppType::Native | AppType::Utility => {
            run_install_commands(&manifest.runtime.install, manifest_dir)?;
            let start = manifest.runtime.start.clone().unwrap_or_default();
            Ok(process_runtime(&start))
        }
    }
}

/// Start command → process runtime. Runs through `sh -c` so packages can use
/// arguments and env references; `${VAR}` substitution arrives with DMN-018.
fn process_runtime(start: &str) -> Runtime {
    Runtime::Process {
        command: "/bin/sh".into(),
        args: vec!["-c".into(), start.to_string()],
    }
}

/// Create (but do not start) the container via the Docker Engine API: ports,
/// env defaults, volumes mapped under `<app_dir>/data/`.
fn docker_create(
    manifest: &Manifest,
    image: &str,
    container: &str,
    app_dir: &Path,
    docker_cfg: &DockerConfig,
) -> Result<()> {
    let env: Vec<String> = manifest
        .env
        .iter()
        .filter_map(|e| {
            e.default
                .as_ref()
                .map(|default| format!("{}={}", e.name, yaml_scalar(default)))
        })
        .collect();

    let mut binds = Vec::new();
    for volume in &manifest.volumes {
        let host = app_dir.join("data").join(volume_dir_name(volume));
        fs::create_dir_all(&host)
            .with_context(|| format!("cannot create volume directory {}", host.display()))?;
        binds.push(format!("{}:{}", host.display(), volume));
    }

    docker::create(
        docker_cfg,
        docker::CreateSpec {
            name: container,
            image,
            env,
            ports: manifest.ports.clone(),
            binds,
        },
    )
    .context("cannot create docker container")
}

/// Host directory name for a container volume path: `/var/lib/data` → `var_lib_data`.
fn volume_dir_name(volume: &str) -> String {
    let name: String = volume
        .trim_matches('/')
        .chars()
        .map(|c| if c == '/' { '_' } else { c })
        .collect();
    if name.is_empty() {
        "volume".into()
    } else {
        name
    }
}

fn yaml_scalar(value: &serde_yaml::Value) -> String {
    match value {
        serde_yaml::Value::String(s) => s.clone(),
        other => serde_yaml::to_string(other)
            .map(|s| s.trim().to_string())
            .unwrap_or_default(),
    }
}

/// Run package install commands (native/utility) from the manifest directory.
fn run_install_commands(commands: &[String], manifest_dir: &Path) -> Result<()> {
    for command in commands {
        info!(command = %command, "running install command");
        let out = Command::new("/bin/sh")
            .args(["-c", command])
            .current_dir(manifest_dir)
            .output()
            .context("cannot run install command")?;
        if !out.status.success() {
            bail!(
                "install command '{}' failed: {}",
                command,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_parsing() {
        assert_eq!(parse_spec("nginx"), ("nginx", None));
        assert_eq!(parse_spec("nginx@1.27.0"), ("nginx", Some("1.27.0")));
        assert_eq!(parse_spec("nginx@"), ("nginx@", None));
    }

    #[test]
    fn volume_names() {
        assert_eq!(volume_dir_name("/data"), "data");
        assert_eq!(volume_dir_name("/var/lib/data"), "var_lib_data");
        assert_eq!(volume_dir_name("/"), "volume");
    }

    #[test]
    fn manifest_dir_rejects_escape() {
        let repo = Path::new("/asc/apps/x/repository");
        assert!(manifest_dir(repo, Some("../../etc")).is_err());
        assert!(manifest_dir(repo, Some("/abs")).is_err());
        assert_eq!(
            manifest_dir(repo, Some("nginx")).unwrap(),
            repo.join("nginx")
        );
    }
}
