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

use super::manifest::{AppType, Manifest, StackManifest};
use super::registry::{RegistryClient, ResolvedPackage};
use super::settings::{SettingValues, SettingsFile};
use crate::daemon::apps::meta::{AppMeta, DesiredState, Owner, Quota, Runtime};
use crate::daemon::apps::{AppStore, UserContext};
use crate::daemon::config::{Config, DockerConfig};
use crate::daemon::docker;
use crate::daemon::i18n::{Msg, t, tf, tf2};

#[derive(Debug)]
pub struct InstallReport {
    pub id: String,
    pub version: String,
}

/// What `asc install <spec>` produced: one app, or the apps of a stack.
#[derive(Debug)]
pub enum InstallOutcome {
    App(InstallReport),
    Stack {
        stack: String,
        /// Freshly installed apps, in dependency (= start) order.
        installed: Vec<InstallReport>,
        /// Stack apps that were already installed and were left untouched.
        skipped: Vec<String>,
    },
}

/// Typed error: several sources provide the requested package. The CLI
/// catches it to let the user pick a source interactively; everyone else
/// sees the candidate list with a hint to pass an explicit source.
#[derive(Debug)]
pub struct AmbiguousPackage {
    pub name: String,
    /// `(source name, git repository)` in source priority order.
    pub candidates: Vec<(String, String)>,
}

impl std::fmt::Display for AmbiguousPackage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let list = self
            .candidates
            .iter()
            .map(|(source, git)| format!("{source} ({git})"))
            .collect::<Vec<_>>()
            .join(", ");
        write!(f, "{}", tf2(Msg::PkgAmbiguous, &self.name, list))
    }
}

impl std::error::Error for AmbiguousPackage {}

/// Pick the candidate from `source`, or the only one there is; several
/// candidates without an explicit source is the [`AmbiguousPackage`] error.
fn select_source(
    mut candidates: Vec<ResolvedPackage>,
    source: Option<&str>,
    package: &str,
) -> Result<ResolvedPackage> {
    if let Some(source) = source {
        return candidates
            .into_iter()
            .find(|c| c.source_name == source)
            .ok_or_else(|| anyhow::anyhow!(tf2(Msg::PkgNotInSource, package, source)));
    }
    if candidates.len() > 1 {
        return Err(anyhow::Error::new(AmbiguousPackage {
            name: package.to_string(),
            candidates: candidates
                .iter()
                .map(|c| (c.source_name.clone(), c.entry.source.git.clone()))
                .collect(),
        }));
    }
    Ok(candidates.remove(0))
}

/// Remove a directory unless `disarm` was called — cleanup for failed installs.
pub(super) struct RemoveOnDrop {
    pub(super) path: PathBuf,
    pub(super) armed: bool,
}

impl RemoveOnDrop {
    pub(super) fn disarm(&mut self) {
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

/// Install from the configured registries. The spec is `name[@version]` for
/// apps and whole stacks, `stack/app[@version]` for one app of a stack.
/// `source` pins the registry source when several provide the package.
pub fn install(
    config: &Config,
    ctx: &UserContext,
    spec: &str,
    source: Option<&str>,
) -> Result<InstallOutcome> {
    let (package_spec, requested_version) = parse_spec(spec);
    let (package, stack_app) = match package_spec.split_once('/') {
        Some((package, app)) if !package.is_empty() && !app.is_empty() => (package, Some(app)),
        Some(_) => bail!("invalid package spec '{package_spec}': use <name> or <stack>/<app>"),
        None => (package_spec, None),
    };

    let candidates = RegistryClient::new(config)?.resolve_all(package)?;
    let resolved = select_source(candidates, source, package)?;
    let version = requested_version
        .map(str::to_string)
        .or_else(|| resolved.entry.latest.clone());

    if resolved.entry.package_type == "stack" {
        return install_stack(config, ctx, &resolved, package, stack_app, version.as_deref());
    }
    if stack_app.is_some() {
        bail!(tf2(Msg::PkgNotAStack, package, package));
    }
    let store = AppStore::new(config.daemon.apps_dir.clone());
    if store.get(package)?.is_some() {
        bail!(tf(Msg::PkgAlreadyInstalled, package));
    }
    install_one(config, ctx, &resolved, package, None, version.as_deref())
        .map(InstallOutcome::App)
}

/// Install a stack: clone once to read `asc.stack.yaml`, then install the
/// selected apps (all non-optional ones, or the requested app) together with
/// their transitive dependencies, dependencies first. Each app installs
/// atomically; already-installed apps are skipped and left untouched.
fn install_stack(
    config: &Config,
    ctx: &UserContext,
    resolved: &ResolvedPackage,
    package: &str,
    stack_app: Option<&str>,
    version: Option<&str>,
) -> Result<InstallOutcome> {
    let probe_dir = std::env::temp_dir().join(format!("asc-stack-{}-{package}", std::process::id()));
    let _ = fs::remove_dir_all(&probe_dir);
    let _probe_cleanup = RemoveOnDrop {
        path: probe_dir.clone(),
        armed: true,
    };
    clone_repository(&resolved.entry.source.git, version, &probe_dir)?;
    let stack_root = manifest_dir(&probe_dir, resolved.entry.source.path.as_deref())?;
    let stack = StackManifest::load(&stack_root)?;

    let wanted: Vec<&str> = match stack_app {
        Some(app) => {
            if stack.app(app).is_none() {
                bail!(tf2(Msg::PkgStackNoApp, package, app));
            }
            vec![app]
        }
        None => stack
            .apps
            .iter()
            .filter(|a| !a.optional)
            .map(|a| a.name.as_str())
            .collect(),
    };

    let store = AppStore::new(config.daemon.apps_dir.clone());
    let mut installed = Vec::new();
    let mut skipped = Vec::new();
    for app in stack.install_order(wanted)? {
        // The app id is the name from the app's own asc.yaml.
        let manifest = Manifest::load(&safe_join(&stack_root, &app.path)?)?;
        if store.get(&manifest.name)?.is_some() {
            skipped.push(manifest.name);
            continue;
        }
        let report = install_one(config, ctx, resolved, &manifest.name, Some(&app.name), version)?;
        installed.push(report);
    }
    Ok(InstallOutcome::Stack {
        stack: package.to_string(),
        installed,
        skipped,
    })
}

/// Install one app: clone the package repository into the app directory,
/// read the manifest (through `asc.stack.yaml` for stack apps), provision
/// the runtime and write meta.json.
fn install_one(
    config: &Config,
    ctx: &UserContext,
    resolved: &ResolvedPackage,
    name: &str,
    stack_app: Option<&str>,
    version: Option<&str>,
) -> Result<InstallReport> {
    let store = AppStore::new(config.daemon.apps_dir.clone());
    let app_dir = store.app_dir(name)?;
    fs::create_dir_all(&app_dir)
        .with_context(|| format!("cannot create app directory {}", app_dir.display()))?;
    let mut cleanup = RemoveOnDrop {
        path: app_dir.clone(),
        armed: true,
    };

    let repo_dir = app_dir.join("repository");
    let cloned_tag = clone_repository(&resolved.entry.source.git, version, &repo_dir)?;

    let (manifest_dir, stack) =
        locate_manifest(&repo_dir, resolved.entry.source.path.as_deref(), stack_app)?;
    let mut manifest = Manifest::load(&manifest_dir)?;
    if let Some(stack) = &stack {
        manifest.merge_stack_env(&stack.env);
    }

    // The app type is only known after reading the manifest; the cleanup
    // guard removes the cloned repository on this failure path too.
    enforce_install_policy(config, ctx, &manifest, name)?;

    let settings = SettingsFile::load_for(&manifest_dir, &manifest)?;
    let quota = load_quota(settings.as_ref())?;

    for sub in ["config", "data"] {
        fs::create_dir_all(app_dir.join(sub))
            .with_context(|| format!("cannot create {sub}/ in app directory"))?;
    }
    // Seed the setting values with the package defaults, so the settings
    // editor (`asc app settings`) and the runtime see a consistent state.
    if let Some(settings) = &settings
        && !settings.settings.is_empty()
    {
        let mut values = SettingValues::default();
        values.merge_defaults(&settings.settings);
        values.save(&app_dir.join("config"))?;
    }

    let runtime = provision(
        &manifest,
        name,
        &app_dir,
        &manifest_dir,
        &config.docker,
        quota.as_ref(),
    )?;

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
        package: stack_app.map(|app| format!("{}/{app}", resolved.entry.name)),
        desired_state: DesiredState::Stopped,
        quota,
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

/// Root policy (DMN-003): regular users may be limited to Docker apps.
pub(super) fn enforce_install_policy(
    config: &Config,
    ctx: &UserContext,
    manifest: &Manifest,
    name: &str,
) -> Result<()> {
    if !ctx.is_root
        && config.policy.user_install == crate::daemon::config::UserInstall::Docker
        && manifest.app_type != AppType::Docker
    {
        bail!(tf(Msg::PkgPolicyDockerOnly, name));
    }
    Ok(())
}

/// `name@1.2.0` → (`name`, Some(`1.2.0`)).
pub(super) fn parse_spec(spec: &str) -> (&str, Option<&str>) {
    match spec.split_once('@') {
        Some((name, version)) if !version.is_empty() => (name, Some(version)),
        _ => (spec, None),
    }
}

/// Clone the package repository; versions are git tags. Returns the tag that
/// was actually checked out (tries `1.2.0`, then `v1.2.0`), or `None` when
/// no version was requested and the default branch HEAD was cloned.
pub(super) fn clone_repository(
    git_url: &str,
    version: Option<&str>,
    dest: &Path,
) -> Result<Option<String>> {
    match version {
        Some(tag) => {
            let candidates = [tag.to_string(), format!("v{tag}")];
            let mut last_err = String::new();
            for candidate in &candidates {
                match git_clone(git_url, Some(candidate), dest) {
                    Ok(()) => return Ok(Some(candidate.clone())),
                    Err(err) => {
                        // A failed clone may leave a partial directory behind.
                        let _ = fs::remove_dir_all(dest);
                        // Missing auth fails for every tag spelling alike:
                        // surface the typed error for the interactive flow.
                        if err.downcast_ref::<super::auth::AuthRequired>().is_some() {
                            return Err(err);
                        }
                        last_err = format!("{err:#}");
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

    // Credentials for private repositories (DMN-003). An unreadable auth
    // file must not block installs from public repositories.
    let auth = match super::auth::GitAuth::load() {
        Ok(auth) => Some(auth),
        Err(err) => {
            warn!(error = %format!("{err:#}"), "cannot read git credentials, cloning without auth");
            None
        }
    };
    let credential = auth.as_ref().and_then(|a| a.lookup(git_url));
    let mut cmd = Command::new("git");
    cmd.args(&args);
    let _askpass = super::auth::configure_git(&mut cmd, credential.map(|c| &c.method))?;

    let out = match cmd.output() {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(t(Msg::ErrGitNotFound)),
        Err(e) => return Err(e).context("cannot run git"),
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        // Only when no credential matched: with one configured, a plain
        // error (with the real git message) beats an offer to reconfigure.
        if credential.is_none() && super::auth::is_auth_failure(&stderr) {
            return Err(anyhow::Error::new(super::auth::AuthRequired {
                url: git_url.to_string(),
            }));
        }
        bail!("git clone failed: {}", stderr.trim());
    }
    Ok(())
}

/// Manifest directory inside the repository (monorepo packages set `path`).
pub(super) fn manifest_dir(repo_dir: &Path, sub: Option<&str>) -> Result<PathBuf> {
    match sub {
        None => Ok(repo_dir.to_path_buf()),
        Some(sub) => safe_join(repo_dir, sub),
    }
}

/// Join a relative path from a registry entry or a stack manifest — never
/// let it escape the repository. `has_root` also catches "/abs", which is
/// not `is_absolute` on Windows.
pub(super) fn safe_join(base: &Path, sub: &str) -> Result<PathBuf> {
    let clean = Path::new(sub);
    if clean.is_absolute()
        || clean.has_root()
        || clean
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        bail!("invalid package path '{sub}'");
    }
    Ok(base.join(clean))
}

/// Directory of the app manifest inside a cloned repository: the registry
/// entry `path`, then — for stack apps — the app path from `asc.stack.yaml`.
/// Returns the stack manifest for stack apps (its shared env is merged into
/// the app manifest by the callers).
pub(super) fn locate_manifest(
    repo_dir: &Path,
    entry_path: Option<&str>,
    stack_app: Option<&str>,
) -> Result<(PathBuf, Option<StackManifest>)> {
    let root = manifest_dir(repo_dir, entry_path)?;
    let Some(app) = stack_app else {
        return Ok((root, None));
    };
    let stack = StackManifest::load(&root)?;
    let entry = stack
        .app(app)
        .with_context(|| format!("stack '{}' has no app '{app}'", stack.name))?;
    let dir = safe_join(&root, &entry.path)?;
    Ok((dir, Some(stack)))
}

/// Normalized quota from a parsed settings file, if any.
pub(super) fn load_quota(settings: Option<&SettingsFile>) -> Result<Option<Quota>> {
    settings
        .and_then(|s| s.quota.as_ref())
        .map(|q| q.normalize())
        .transpose()
}

/// Prepare the runtime described by the manifest and return its meta form.
/// The quota is enforced for Docker at container creation; native/process
/// runtimes record it in meta.json (cgroup enforcement is a next increment).
pub(super) fn provision(
    manifest: &Manifest,
    id: &str,
    app_dir: &Path,
    manifest_dir: &Path,
    docker_cfg: &DockerConfig,
    quota: Option<&Quota>,
) -> Result<Runtime> {
    match manifest.app_type {
        AppType::Docker => {
            let image = manifest
                .runtime
                .image
                .as_deref()
                .expect("validated: docker manifests have an image");
            let container = format!("asc-{id}");
            docker_create(manifest, image, &container, app_dir, docker_cfg, quota)?;
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
/// env defaults, volumes mapped under `<app_dir>/data/`, quota limits.
fn docker_create(
    manifest: &Manifest,
    image: &str,
    container: &str,
    app_dir: &Path,
    docker_cfg: &DockerConfig,
    quota: Option<&Quota>,
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
            // 1 core = 1e9 NanoCpus (Docker's own `--cpus` scale).
            nano_cpus: quota
                .and_then(|q| q.cpu_cores)
                .map(|cores| (cores * 1_000_000_000.0) as i64),
            memory_bytes: quota.and_then(|q| q.ram_bytes).map(|bytes| bytes as i64),
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
