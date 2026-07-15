//! Install flow: resolve package → clone repository → read manifest →
//! provision runtime → write meta.json.
//!
//! Installing is atomic from the user's point of view: any failure removes
//! the half-created app directory, so a retry starts clean.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use super::manifest::{AppType, Manifest, StackManifest};
use super::registry::{RegistryClient, ResolvedPackage};
use super::settings::{SettingKind, SettingValues, SettingsFile};
use crate::daemon::apps::meta::{AppMeta, DesiredState, Owner, Quota, Runtime};
use crate::daemon::apps::{AppStore, UserContext};
use crate::daemon::config::{Config, DockerConfig};
use crate::daemon::docker;
use crate::daemon::i18n::{Msg, t, tf, tf2, tf3};
use crate::daemon::progress::{self, PhaseBar};

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

/// Typed error: the package repository ships a license the caller has not
/// accepted yet. The CLI catches it, shows the source, the repository and
/// the license text, and retries with acceptance; the platform UI renders
/// its own consent dialog from the same fields.
#[derive(Debug)]
pub struct LicenseRequired {
    pub package: String,
    pub source: String,
    pub git: String,
    /// The license text (LICENSE.md / LICENSE / LICENSE.txt at the repo root).
    pub license: String,
}

impl std::fmt::Display for LicenseRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            tf3(
                Msg::PkgLicenseRequired,
                &self.package,
                &self.source,
                &self.git
            )
        )
    }
}

impl std::error::Error for LicenseRequired {}

/// License text of a cloned package repository: the first of LICENSE.md,
/// LICENSE, LICENSE.txt at the repository root. `None` — no license file.
fn repo_license(repo_dir: &Path) -> Option<String> {
    ["LICENSE.md", "LICENSE", "LICENSE.txt"]
        .iter()
        .find_map(|name| fs::read_to_string(repo_dir.join(name)).ok())
}

/// Bail with [`LicenseRequired`] when the repository ships a license and the
/// caller has not accepted it. Repositories without a license file install
/// without a prompt.
fn require_license_ack(
    resolved: &ResolvedPackage,
    package: &str,
    repo_dir: &Path,
    license_ack: bool,
) -> Result<()> {
    if license_ack {
        return Ok(());
    }
    if let Some(license) = repo_license(repo_dir) {
        return Err(anyhow::Error::new(LicenseRequired {
            package: package.to_string(),
            source: resolved.source_name.clone(),
            git: resolved.entry.source.git.clone(),
            license,
        }));
    }
    Ok(())
}

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
/// `custom_name` is the user-chosen app name (DMN-024); it applies to a
/// single app — installing a whole stack with one rejects the install.
/// `license_ack` — the caller has accepted the repository license; without
/// it a repository shipping a LICENSE raises [`LicenseRequired`].
pub fn install(
    config: &Config,
    ctx: &UserContext,
    spec: &str,
    source: Option<&str>,
    custom_name: Option<&str>,
    license_ack: bool,
) -> Result<InstallOutcome> {
    let (package_spec, requested_version) = parse_spec(spec);
    let (package, stack_app) = match package_spec.split_once('/') {
        Some((package, app)) if !package.is_empty() && !app.is_empty() => (package, Some(app)),
        Some(_) => bail!("invalid package spec '{package_spec}': use <name> or <stack>/<app>"),
        None => (package_spec, None),
    };
    if let Some(name) = custom_name {
        validate_custom_name(config, ctx, name)?;
    }

    let candidates = RegistryClient::new(config)?.resolve_all(package)?;
    let resolved = select_source(candidates, source, package)?;
    let version = requested_version
        .map(str::to_string)
        .or_else(|| resolved.entry.latest.clone());

    let opts = InstallOpts {
        version: version.as_deref(),
        custom_name,
        license_ack,
    };
    if resolved.entry.package_type == "stack" {
        if custom_name.is_some() && stack_app.is_none() {
            bail!(tf(Msg::PkgNameForSingleApp, package));
        }
        return install_stack(config, ctx, &resolved, package, stack_app, opts);
    }
    if stack_app.is_some() {
        bail!(tf2(Msg::PkgNotAStack, package, package));
    }
    let store = AppStore::new(config.daemon.apps_dir.clone());
    if store.get(package)?.is_some() {
        bail!(tf(Msg::PkgAlreadyInstalled, package));
    }
    install_one(config, ctx, &resolved, package, None, opts).map(InstallOutcome::App)
}

/// Options shared by every install path.
#[derive(Clone, Copy)]
struct InstallOpts<'a> {
    version: Option<&'a str>,
    custom_name: Option<&'a str>,
    license_ack: bool,
}

/// Validate a user-chosen app name (DMN-024): printable, sane length, and
/// unique among the apps this user can see — otherwise `asc app <name>`
/// commands would be ambiguous. Uniqueness is checked against visible apps
/// only, so the error never leaks foreign users' app names.
fn validate_custom_name(config: &Config, ctx: &UserContext, name: &str) -> Result<()> {
    let ok_len = (1..=64).contains(&name.chars().count());
    let ok_chars = !name.chars().any(char::is_control);
    if !ok_len || !ok_chars || name.trim() != name {
        bail!(tf(Msg::PkgNameInvalid, name));
    }
    let store = AppStore::new(config.daemon.apps_dir.clone());
    for meta in store.list()? {
        if !ctx.is_root && meta.owner.uid != ctx.uid {
            continue;
        }
        if meta.id == name || meta.custom_name.as_deref() == Some(name) {
            bail!(tf(Msg::PkgNameTaken, name));
        }
    }
    Ok(())
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
    opts: InstallOpts<'_>,
) -> Result<InstallOutcome> {
    let probe_dir =
        std::env::temp_dir().join(format!("asc-stack-{}-{package}", std::process::id()));
    let _ = fs::remove_dir_all(&probe_dir);
    let _probe_cleanup = RemoveOnDrop {
        path: probe_dir.clone(),
        armed: true,
    };
    clone_repository(&resolved.entry.source.git, opts.version, &probe_dir)?;
    // One repository = one license: consent is asked once for the stack.
    require_license_ack(resolved, package, &probe_dir, opts.license_ack)?;
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
        // The custom name goes to the app the user asked for, not to the
        // dependencies pulled in alongside it; the license was accepted for
        // the whole repository above.
        let app_opts = InstallOpts {
            custom_name: opts
                .custom_name
                .filter(|_| Some(app.name.as_str()) == stack_app),
            license_ack: true,
            ..opts
        };
        let report = install_one(
            config,
            ctx,
            resolved,
            &manifest.name,
            Some(&app.name),
            app_opts,
        )?;
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
    opts: InstallOpts<'_>,
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
    let cloned_tag = clone_repository(&resolved.entry.source.git, opts.version, &repo_dir)?;
    require_license_ack(resolved, name, &repo_dir, opts.license_ack)?;

    let (manifest_dir, _) =
        locate_manifest(&repo_dir, resolved.entry.source.path.as_deref(), stack_app)?;
    let manifest = Manifest::load(&manifest_dir)?;

    // The app type is only known after reading the manifest; the cleanup
    // guard removes the cloned repository on this failure path too.
    enforce_install_policy(config, ctx, &manifest, name)?;

    let settings = SettingsFile::load_for(&manifest_dir, &manifest)?;
    let quota = load_quota(settings.as_ref(), &app_dir.join("config"))?;

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
        settings.as_ref(),
    )?;

    let effective_version = cloned_tag.unwrap_or_else(|| manifest.version.clone());
    let meta = AppMeta {
        id: name.to_string(),
        name: manifest.title.clone().unwrap_or_else(|| name.to_string()),
        custom_name: opts.custom_name.map(str::to_string),
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

/// Clones with `git`'s own `--progress` output rendered as a live bar on a
/// terminal (regardless of the log level): `git` normally suppresses that
/// output once stderr isn't a tty (which it never is here, piped for
/// capture), so `--progress` forces it back on and [`read_progress_lines`]
/// parses the `\r`-delimited status line as it streams in.
fn git_clone(git_url: &str, tag: Option<&str>, dest: &Path) -> Result<()> {
    let mut args: Vec<&str> = vec!["clone", "--depth", "1", "--progress"];
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
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::piped());
    let _askpass = super::auth::configure_git(&mut cmd, credential.map(|c| &c.method))?;

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(t(Msg::ErrGitNotFound)),
        Err(e) => return Err(e).context("cannot run git"),
    };
    // Read to EOF before wait(): git closes stderr at exit, and draining it
    // first avoids the classic pipe-fills-up-then-deadlock ordering bug.
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let bar = progress::interactive().then(PhaseBar::new);
    let captured = read_progress_lines(&mut stderr, |line| {
        if let (Some(bar), Some((phase, pct))) = (&bar, progress::parse_git_progress(line)) {
            bar.update(phase, pct);
        }
    });
    if let Some(bar) = bar {
        bar.finish();
    }
    let status = child.wait().context("cannot wait for git clone")?;
    if !status.success() {
        // Only when no credential matched: with one configured, a plain
        // error (with the real git message) beats an offer to reconfigure.
        if credential.is_none() && super::auth::is_auth_failure(&captured) {
            return Err(anyhow::Error::new(super::auth::AuthRequired {
                url: git_url.to_string(),
            }));
        }
        bail!("git clone failed: {}", captured.trim());
    }
    Ok(())
}

/// Read a child's piped stderr, splitting on `\r` *or* `\n` — `git
/// --progress` overwrites its status line with a bare `\r`, which
/// `BufRead::lines()` would never split on. Calls `on_line` for every
/// segment and returns the full text, newline-joined, for error reporting.
fn read_progress_lines(reader: &mut impl Read, mut on_line: impl FnMut(&str)) -> String {
    let mut captured = String::new();
    let mut pending = Vec::new();
    let mut buf = [0u8; 4096];
    let flush = |pending: &mut Vec<u8>, captured: &mut String, on_line: &mut dyn FnMut(&str)| {
        if pending.is_empty() {
            return;
        }
        let line = String::from_utf8_lossy(pending).into_owned();
        on_line(&line);
        if !captured.is_empty() {
            captured.push('\n');
        }
        captured.push_str(&line);
        pending.clear();
    };
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for &byte in &buf[..n] {
            if byte == b'\r' || byte == b'\n' {
                flush(&mut pending, &mut captured, &mut on_line);
            } else {
                pending.push(byte);
            }
        }
    }
    flush(&mut pending, &mut captured, &mut on_line);
    captured
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

/// Normalized quota: the package `quota:` section with the user's `$quota`
/// overrides (the quota category of `asc app settings`) applied field-wise.
pub(super) fn load_quota(
    settings: Option<&SettingsFile>,
    config_dir: &Path,
) -> Result<Option<Quota>> {
    let package = settings.and_then(|s| s.quota.as_ref());
    let user = SettingValues::load(config_dir)?.quota_override()?;
    let merged = match (package, user) {
        (None, None) => return Ok(None),
        (Some(package), None) => package.clone(),
        (None, Some(user)) => user,
        (Some(package), Some(user)) => super::settings::QuotaSpec {
            max_cpu: user.max_cpu.or(package.max_cpu),
            max_ram: user.max_ram.clone().or_else(|| package.max_ram.clone()),
            max_disk: user.max_disk.clone().or_else(|| package.max_disk.clone()),
        },
    };
    merged.normalize().map(Some)
}

/// Prepare the runtime described by the manifest and return its meta form.
/// The quota is enforced for Docker at container creation; native/process
/// runtimes record it in meta.json (cgroup enforcement is a next increment).
/// Everything user-adjustable comes from the settings (asc.settings.yaml +
/// settings.json): env, published ports, volumes, the quota and the start
/// command — asc.yaml describes only the runtime itself.
pub(super) fn provision(
    manifest: &Manifest,
    id: &str,
    app_dir: &Path,
    manifest_dir: &Path,
    docker_cfg: &DockerConfig,
    quota: Option<&Quota>,
    settings: Option<&SettingsFile>,
) -> Result<Runtime> {
    let inputs = runtime_inputs(settings, &app_dir.join("config"))?;
    let start_command = inputs
        .start_command
        .as_deref()
        .map(|c| interpolate_env(c, &inputs.env))
        .transpose()?;
    match manifest.app_type {
        AppType::Docker => {
            let container = format!("asc-{id}");
            docker_create(
                manifest,
                &inputs,
                &container,
                app_dir,
                docker_cfg,
                quota,
                start_command,
            )?;
            Ok(Runtime::Docker { container })
        }
        AppType::Native | AppType::Utility => {
            run_install_commands(&manifest.runtime.install, manifest_dir)?;
            let start = start_command
                .or_else(|| manifest.runtime.start.clone())
                .unwrap_or_default();
            Ok(process_runtime(&start))
        }
    }
}

/// Substitute `${VAR}` references in a start_command with the app's env
/// (setting values with `env:` keys, defaults included). A reference to an
/// unknown variable — or one without a value — fails the install: a typo
/// must not reach the runtime as a broken command.
pub(super) fn interpolate_env(command: &str, env: &[(String, String)]) -> Result<String> {
    let mut out = String::with_capacity(command.len());
    let mut rest = command;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            bail!("start_command has an unterminated ${{...}} reference");
        };
        let name = &after[..end];
        let value = env
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
            .with_context(|| {
                format!(
                    "start_command references ${{{name}}}, which no setting provides (asc.settings.yaml `env:`)"
                )
            })?;
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Start command → process runtime. Runs through `sh -c` so packages can use
/// arguments and env references; `${VAR}` substitution arrives with DMN-018.
fn process_runtime(start: &str) -> Runtime {
    Runtime::Process {
        command: "/bin/sh".into(),
        args: vec!["-c".into(), start.to_string()],
    }
}

/// Everything the runtime takes from the settings (DMN-017, DMN-030): env
/// pairs, published ports, volume entries and the effective start command.
/// Values come from `<config_dir>/settings.json` with the package defaults
/// filled in for keys the user has not set — so the inputs are complete even
/// before the first settings edit. The settings are the **only** source:
/// asc.yaml has no `env:`, `ports:` or `volumes:` sections.
pub(super) struct RuntimeInputs {
    /// `(ENV_NAME, value)` for every setting with an `env:` key.
    pub env: Vec<(String, String)>,
    /// Flattened values of every `type: ports` setting, each with its
    /// transport(s) (`protocol:`, default `tcp`).
    pub ports: Vec<(u16, docker::PortProtocol)>,
    /// Flattened values of every `type: volumes` setting.
    pub volumes: Vec<String>,
    /// The user's `$start_command` override, else the package's.
    pub start_command: Option<String>,
}

pub(super) fn runtime_inputs(
    settings: Option<&SettingsFile>,
    config_dir: &Path,
) -> Result<RuntimeInputs> {
    let mut values = SettingValues::load(config_dir)?;
    let Some(settings) = settings else {
        return Ok(RuntimeInputs {
            env: Vec::new(),
            ports: Vec::new(),
            volumes: Vec::new(),
            start_command: values.start_command_override().map(str::to_string),
        });
    };
    values.merge_defaults(&settings.settings);
    let mut ports = Vec::new();
    let mut volumes = Vec::new();
    for def in &settings.settings {
        let Some(value) = values.get(&def.key) else {
            continue;
        };
        match def.kind {
            SettingKind::Ports => {
                let list = value
                    .as_array()
                    .with_context(|| format!("setting '{}' must be a list of ports", def.key))?;
                for item in list {
                    let port = item
                        .as_u64()
                        .and_then(|p| u16::try_from(p).ok())
                        .filter(|p| *p != 0)
                        .with_context(|| format!("setting '{}': invalid port {item}", def.key))?;
                    ports.push((port, def.port_protocol()));
                }
            }
            SettingKind::Volumes => {
                let list = value
                    .as_array()
                    .with_context(|| format!("setting '{}' must be a list of volumes", def.key))?;
                for item in list {
                    let entry = item
                        .as_str()
                        .with_context(|| format!("setting '{}': invalid volume {item}", def.key))?;
                    volumes.push(entry.to_string());
                }
            }
            _ => {}
        }
    }
    let start_command = values
        .start_command_override()
        .map(str::to_string)
        .or_else(|| settings.start_command.clone());
    Ok(RuntimeInputs {
        env: values.env_pairs(&settings.settings),
        ports,
        volumes,
        start_command,
    })
}

/// Create (but do not start) the container via the Docker Engine API: env,
/// published ports, volumes and quota limits all come from the settings.
fn docker_create(
    manifest: &Manifest,
    inputs: &RuntimeInputs,
    container: &str,
    app_dir: &Path,
    docker_cfg: &DockerConfig,
    quota: Option<&Quota>,
    command: Option<String>,
) -> Result<()> {
    let image = manifest
        .runtime
        .image
        .as_deref()
        .expect("validated: docker manifests have an image");
    let env: Vec<String> = inputs
        .env
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect();

    let mut binds = Vec::new();
    for volume in &inputs.volumes {
        binds.push(volume_bind(volume, app_dir)?);
    }

    docker::create(
        docker_cfg,
        docker::CreateSpec {
            name: container,
            image,
            env,
            ports: inputs.ports.clone(),
            binds,
            // 1 core = 1e9 NanoCpus (Docker's own `--cpus` scale).
            nano_cpus: quota
                .and_then(|q| q.cpu_cores)
                .map(|cores| (cores * 1_000_000_000.0) as i64),
            memory_bytes: quota.and_then(|q| q.ram_bytes).map(|bytes| bytes as i64),
            command,
            open_stdin: manifest.runtime.stdin,
            tty: manifest.runtime.tty,
        },
    )
    .context("cannot create docker container")
}

/// Host folder names an app volume may not take: they are the daemon's own
/// files inside the app directory.
const RESERVED_VOLUME_DIRS: [&str; 3] = ["repository", "config", "meta.json"];

/// One parsed volume entry. Three forms are supported:
///
/// - `/container/path` — private app data: mounted from the `data` folder of
///   the app directory (`/asc/apps/<id>/data`);
/// - `/container/path:host` — same, but the host side after the colon is
///   used instead of `data`: a plain folder name lands inside the app
///   directory (`/asc/apps/<id>/<folder>`), an absolute path is a host
///   machine path used verbatim;
/// - `name:/container/path[:ro|:rw]` — a Docker **named volume**, passed to
///   the Engine verbatim (it creates the volume on first use). Named volumes
///   are how several apps share data — e.g. one game-files volume written by
///   a master app and mounted read-only by every server instance.
enum Volume<'a> {
    /// `(container path, host side)`.
    Private(&'a str, HostSide<'a>),
    Named(&'a str),
}

enum HostSide<'a> {
    /// A folder inside the app directory (`data` by default).
    AppFolder(&'a str),
    /// An absolute host path, mounted verbatim.
    HostPath(&'a str),
}

/// Syntax check shared by the installer and the settings editor: parses one
/// volume entry without touching the filesystem.
pub(super) fn validate_volume(volume: &str) -> Result<()> {
    parse_volume(volume).map(|_| ())
}

fn parse_volume(volume: &str) -> Result<Volume<'_>> {
    let invalid = || {
        anyhow::anyhow!(
            "invalid volume '{volume}': expected /container/path[:host_folder|:/host/path] \
             or name:/container/path[:ro]"
        )
    };
    if volume.starts_with('/') {
        let (target, host) = match volume.split_once(':') {
            Some((target, host)) => (target, host),
            None => (volume, "data"),
        };
        if target.len() < 2 {
            return Err(invalid());
        }
        if host.starts_with('/') {
            // An absolute host path: reject traversal, require a real path.
            let clean = Path::new(host);
            if host.len() < 2
                || clean
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err(invalid());
            }
            return Ok(Volume::Private(target, HostSide::HostPath(host)));
        }
        let folder_ok = !host.is_empty()
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
            && host != "."
            && host != ".."
            && !RESERVED_VOLUME_DIRS.contains(&host);
        if !folder_ok {
            return Err(invalid());
        }
        return Ok(Volume::Private(target, HostSide::AppFolder(host)));
    }
    let (name, target) = volume.split_once(':').ok_or_else(invalid)?;
    // Docker volume names: [a-zA-Z0-9][a-zA-Z0-9_.-]*
    let name_ok = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    let path = match target.rsplit_once(':') {
        Some((path, mode)) if mode == "ro" || mode == "rw" => path,
        _ => target,
    };
    if !name_ok || !path.starts_with('/') {
        return Err(invalid());
    }
    Ok(Volume::Named(volume))
}

/// Bind string for one volume entry; creates the host directory when needed.
pub(super) fn volume_bind(volume: &str, app_dir: &Path) -> Result<String> {
    let (host, target) = match parse_volume(volume)? {
        Volume::Named(entry) => return Ok(entry.to_string()),
        Volume::Private(target, HostSide::AppFolder(folder)) => (app_dir.join(folder), target),
        Volume::Private(target, HostSide::HostPath(path)) => {
            let host = PathBuf::from(path);
            // A pre-existing host directory keeps its ownership and mode —
            // the admin manages it; only a freshly created one is opened up
            // for the container's arbitrary uid.
            if !host.exists() {
                fs::create_dir_all(&host).with_context(|| {
                    format!("cannot create volume directory {}", host.display())
                })?;
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&host, fs::Permissions::from_mode(0o777))
                    .with_context(|| format!("cannot chmod volume directory {}", host.display()))?;
            }
            return Ok(format!("{path}:{target}"));
        }
    };
    fs::create_dir_all(&host)
        .with_context(|| format!("cannot create volume directory {}", host.display()))?;
    // Images run under arbitrary non-root uids (`steam`, `www-data`, …)
    // and bind mounts keep host ownership — a root-owned 0755 directory
    // is read-only for them. World-writable applies to this leaf
    // directory only; the app directory above stays restrictive.
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&host, fs::Permissions::from_mode(0o777))
            .with_context(|| format!("cannot chmod volume directory {}", host.display()))?;
    }
    Ok(format!("{}:{}", host.display(), target))
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
    fn plain_volumes_bind_the_data_folder() {
        let dir = tempfile::tempdir().unwrap();
        let bind = volume_bind("/home/steam/cs2-dedicated", dir.path()).unwrap();
        let host = dir.path().join("data");
        assert_eq!(
            bind,
            format!("{}:/home/steam/cs2-dedicated", host.display())
        );
        assert!(host.is_dir(), "the host directory must be created");
        // Container images run under arbitrary uids: the data directory
        // must be writable for them despite bind-mount host ownership.
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&host).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o777, "volume dir must be world-writable");
    }

    #[test]
    fn volume_host_folder_after_the_colon_replaces_data() {
        let dir = tempfile::tempdir().unwrap();
        let bind = volume_bind("/var/lib/postgresql:pgdata", dir.path()).unwrap();
        let host = dir.path().join("pgdata");
        assert_eq!(bind, format!("{}:/var/lib/postgresql", host.display()));
        assert!(host.is_dir(), "the host directory must be created");

        // An absolute path after the colon is a host machine path, verbatim.
        let host_dir = dir.path().join("srv").join("maps");
        let entry = format!("/opt/maps:{}", host_dir.display());
        let bind = volume_bind(&entry, dir.path()).unwrap();
        assert_eq!(bind, format!("{}:/opt/maps", host_dir.display()));
        assert!(host_dir.is_dir(), "the host directory must be created");

        // Folder names that would collide with the daemon's own files — or
        // escape the app directory — are rejected; so is host-path traversal.
        for bad in [
            "/data:repository",
            "/data:config",
            "/data:meta.json",
            "/data:..",
            "/data:a/b",
            "/data:",
            "/data:/srv/../etc",
        ] {
            assert!(volume_bind(bad, dir.path()).is_err(), "must reject '{bad}'");
        }
    }

    #[test]
    fn named_volumes_pass_through_verbatim() {
        let app = Path::new("/nonexistent");
        assert_eq!(
            volume_bind("cs2-master-data:/data", app).unwrap(),
            "cs2-master-data:/data"
        );
        assert_eq!(
            volume_bind("cs2-master-data:/home/steam/cs2-dedicated:ro", app).unwrap(),
            "cs2-master-data:/home/steam/cs2-dedicated:ro"
        );
        for bad in ["vol", "vol:data", ":/data", "-vol:/data", "a b:/data"] {
            assert!(volume_bind(bad, app).is_err(), "must reject '{bad}'");
        }
    }

    #[test]
    fn start_command_interpolates_settings_env() {
        let env = [
            ("STEAM_APP_ID".to_string(), "730".to_string()),
            ("DIR".to_string(), "/data".to_string()),
        ];
        assert_eq!(
            interpolate_env(
                "steamcmd +force_install_dir ${DIR} +app_update ${STEAM_APP_ID} +quit",
                &env
            )
            .unwrap(),
            "steamcmd +force_install_dir /data +app_update 730 +quit"
        );
        assert_eq!(interpolate_env("no refs", &env).unwrap(), "no refs");
        // Unknown vars must fail, not launch a broken command.
        assert!(interpolate_env("run ${MISSING}", &env).is_err());
        assert!(interpolate_env("run ${UNTERMINATED", &env).is_err());
    }

    #[test]
    fn settings_are_the_only_runtime_input_source() {
        use super::super::settings::SettingValues;
        let settings: SettingsFile = serde_yaml::from_str(
            "settings:\n  - { key: map, type: enum, values: [de_dust2, de_mirage], default: de_dust2, env: CS2_STARTMAP }\n  - { key: rcon, type: secret, env: CS2_RCONPW }\n  - { key: game_port, type: ports, default: [27015], env: CS2_PORT }\n  - { key: game_data, type: volumes, default: [/home/steam/cs2-dedicated] }\n",
        )
        .unwrap();
        settings.validate().unwrap();
        let dir = tempfile::tempdir().unwrap();

        // No values chosen yet: defaults fill in; valueless settings are
        // absent. A single-port list reaches env as the bare number.
        let inputs = runtime_inputs(Some(&settings), dir.path()).unwrap();
        assert_eq!(
            inputs.env,
            [
                ("CS2_STARTMAP".to_string(), "de_dust2".to_string()),
                ("CS2_PORT".to_string(), "27015".to_string())
            ]
        );
        assert_eq!(inputs.ports, [(27015, docker::PortProtocol::Tcp)]);
        assert_eq!(inputs.volumes, ["/home/steam/cs2-dedicated"]);
        assert_eq!(inputs.start_command, None);

        // User-chosen values win over the defaults; the `$start_command`
        // override becomes the effective start command.
        let mut values = SettingValues::load(dir.path()).unwrap();
        values.set("map", serde_json::json!("de_mirage"));
        values.set("rcon", serde_json::json!("hunter2"));
        values.set("game_port", serde_json::json!([27016, 27017]));
        values.set(
            SettingValues::START_COMMAND_KEY,
            serde_json::json!("./cs2 -override"),
        );
        values.save(dir.path()).unwrap();
        let inputs = runtime_inputs(Some(&settings), dir.path()).unwrap();
        assert_eq!(
            inputs.env,
            [
                ("CS2_STARTMAP".to_string(), "de_mirage".to_string()),
                ("CS2_RCONPW".to_string(), "hunter2".to_string()),
                ("CS2_PORT".to_string(), "27016,27017".to_string())
            ]
        );
        assert_eq!(
            inputs.ports,
            [
                (27016, docker::PortProtocol::Tcp),
                (27017, docker::PortProtocol::Tcp)
            ]
        );
        assert_eq!(inputs.start_command.as_deref(), Some("./cs2 -override"));

        // No settings file — nothing but the overrides.
        let inputs = runtime_inputs(None, dir.path()).unwrap();
        assert!(inputs.env.is_empty() && inputs.ports.is_empty() && inputs.volumes.is_empty());
        assert_eq!(inputs.start_command.as_deref(), Some("./cs2 -override"));
    }

    #[test]
    fn quota_overrides_win_field_wise() {
        use super::super::settings::SettingValues;
        let settings: SettingsFile =
            serde_yaml::from_str("quota:\n  max_cpu: 2\n  max_ram: 4G\n  max_disk: 100G\n")
                .unwrap();
        let dir = tempfile::tempdir().unwrap();

        // Package quota alone.
        let quota = load_quota(Some(&settings), dir.path()).unwrap().unwrap();
        assert_eq!(quota.cpu_cores, Some(2.0));
        assert_eq!(quota.ram_bytes, Some(4 << 30));

        // A $quota override replaces only the fields it sets.
        let mut values = SettingValues::load(dir.path()).unwrap();
        values.set(
            SettingValues::QUOTA_KEY,
            serde_json::json!({ "max_ram": "8G" }),
        );
        values.save(dir.path()).unwrap();
        let quota = load_quota(Some(&settings), dir.path()).unwrap().unwrap();
        assert_eq!(quota.cpu_cores, Some(2.0), "package field survives");
        assert_eq!(quota.ram_bytes, Some(8 << 30), "override wins");
        assert_eq!(quota.disk_bytes, Some(100 << 30));

        // An override without a package quota still limits the app.
        let quota = load_quota(None, dir.path()).unwrap().unwrap();
        assert_eq!(quota.ram_bytes, Some(8 << 30));
        assert_eq!(quota.cpu_cores, None);
    }

    #[test]
    fn start_command_overrides_native_start() {
        let dir = tempfile::tempdir().unwrap();
        let manifest: Manifest = serde_yaml::from_str(
            "name: tool\nversion: '1'\ntype: native\nruntime:\n  start: ./run.sh\n",
        )
        .unwrap();
        let docker_cfg = DockerConfig {
            socket: dir.path().join("docker.sock"),
        };
        let settings: SettingsFile = serde_yaml::from_str(
            "start_command: serve --port ${PORT}\nsettings:\n  - { key: port, type: number, default: 8080, env: PORT }\n",
        )
        .unwrap();
        let runtime = provision(
            &manifest,
            "tool",
            dir.path(),
            dir.path(),
            &docker_cfg,
            None,
            Some(&settings),
        )
        .unwrap();
        match runtime {
            Runtime::Process { args, .. } => {
                assert_eq!(args, ["-c", "serve --port 8080"]);
            }
            other => panic!("expected a process runtime, got {other:?}"),
        }
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
