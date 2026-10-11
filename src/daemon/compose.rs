//! `docker compose` orchestration — the one deliberate
//! exception to "Docker only through bollard, never the CLI" (see
//! `AGENTS.md`/`CLAUDE.md`). A compose *project* — variable interpolation,
//! `env_file`, anchors, `extends`, profiles, several override files,
//! `depends_on` with healthcheck waits, per-service networks with DNS
//! aliases, `configs`/`secrets` — is something this daemon deliberately does
//! not model itself: the `docker compose` CLI plugin (v2) already does, so it
//! is shelled out to as an orchestrator, never modeled on top of the Engine
//! API the way ASC's own apps are.
//!
//! Every call that needs the project model lives here and nowhere else:
//! `config`, `pull`, `build`, `up` (creating and recreating containers) and
//! the multiplexed logs of several replicas or of the whole project.
//! Anything that addresses one container — exec, one replica's logs,
//! start/stop/restart/kill of existing containers, stats, inspect, image
//! tags — goes through bollard ([`crate::daemon::docker`]), which finds a
//! project's containers by the labels compose writes on them.
//!
//! The flags used below are chosen to work on every compose v2 release
//! without branching on the version: `config --services`, `config -q`,
//! `pull <svc…>` instead of the later `pull --ignore-buildable`, `up
//! --no-start` instead of `create` (whose flags changed during v2), and a
//! `ps` parser that accepts both the JSON array early v2 printed and the
//! NDJSON of later releases.
//!
//! The plugin is optional: [`available`] probes it once at first use and
//! caches the answer (spawning a process on every capability check would be
//! wasteful, and the answer cannot change while the daemon is running). When
//! it is missing, `docker_compose` stays a **detected, not installable**
//! method — the same "found, not supported" shape Swarm/Kubernetes already
//! use — rather than the caller ever reaching the commands below.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::daemon::config::DockerConfig;
use crate::daemon::progress::{self, InstallReporter};

/// Lines of the plugin's stderr kept for the error of a failed streamed
/// command: enough for a build error and the step that produced it.
const ERROR_TAIL_LINES: usize = 15;

/// One compose project as the plugin addresses it: `-p <project>`, the `-f`
/// files in order, and the directory the command runs from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeProject {
    /// Compose project name (`asc-<id>`).
    pub project: String,
    /// Directory the plugin runs from (the compose file's own directory),
    /// so relative paths inside the files — `build: .`, bind mounts,
    /// `env_file:` — resolve the way the package author wrote them.
    pub working_dir: PathBuf,
    /// `-f` files: relative to `working_dir`, or absolute (the daemon's own
    /// override file, always last).
    pub files: Vec<String>,
}

impl ComposeProject {
    pub fn new(
        project: impl Into<String>,
        working_dir: impl Into<PathBuf>,
        files: Vec<String>,
    ) -> Self {
        Self {
            project: project.into(),
            working_dir: working_dir.into(),
            files,
        }
    }

    /// The same project with one more `-f` file, given last — the place
    /// the daemon's override file always takes, so it wins over the
    /// package's own files.
    pub fn with_file(&self, file: &Path) -> Self {
        let mut project = self.clone();
        project.files.push(file.to_string_lossy().into_owned());
        project
    }
}

fn docker_host(docker: &DockerConfig) -> String {
    format!("unix://{}", docker.socket.display())
}

/// Base `docker compose --ansi never -p <project> -f <file> ...`
/// invocation, run from the project's `working_dir`. `DOCKER_HOST` points
/// the plugin at the configured Engine socket, the same one bollard uses
/// everywhere else in this daemon, rather than whatever the ambient
/// environment happens to have. `--ansi never` keeps colour codes out of
/// both the captured output and a streamed task log.
fn command(docker: &DockerConfig, project: &ComposeProject) -> Command {
    let mut cmd = Command::new("docker");
    cmd.env("DOCKER_HOST", docker_host(docker));
    cmd.current_dir(&project.working_dir);
    cmd.args(["compose", "--ansi", "never", "-p", &project.project]);
    for file in &project.files {
        cmd.arg("-f").arg(file);
    }
    cmd
}

fn run(mut cmd: Command, action: &str) -> Result<String> {
    let output = cmd
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("cannot run docker compose {action}"))?;
    if !output.status.success() {
        bail!(
            "docker compose {action} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Run a compose command with its output streamed into `report` line by
/// line — stdout and stderr both, since compose prints its progress (and a
/// build's `BUILDKIT_PROGRESS=plain` steps) on stderr. A caller that has
/// gone away ([`InstallReporter::cancelled`]) gets the plugin killed, the
/// same way a `git clone` is, and a [`progress::Cancelled`] error back. A
/// failure carries the tail of stderr, which is where compose and BuildKit
/// put the reason.
pub fn run_streaming(
    mut cmd: Command,
    action: &str,
    report: Option<&dyn InstallReporter>,
) -> Result<()> {
    cmd.env("BUILDKIT_PROGRESS", "plain")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    progress::ensure_not_cancelled(report)?;
    if let Some(report) = report {
        report.line(&format!("$ docker {}", display_args(&cmd)));
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("cannot run docker compose {action}"))?;

    // One reader thread per pipe, so neither can fill up and stall the
    // plugin while the other one is being drained.
    let (tx, rx) = mpsc::channel::<(bool, String)>();
    let mut readers = Vec::new();
    if let Some(mut out) = child.stdout.take() {
        let tx = tx.clone();
        readers.push(std::thread::spawn(move || {
            progress::read_progress_lines(&mut out, |line| {
                let _ = tx.send((false, line.to_string()));
            });
        }));
    }
    if let Some(mut err) = child.stderr.take() {
        let tx = tx.clone();
        readers.push(std::thread::spawn(move || {
            progress::read_progress_lines(&mut err, |line| {
                let _ = tx.send((true, line.to_string()));
            });
        }));
    }
    drop(tx);

    let mut tail: VecDeque<String> = VecDeque::with_capacity(ERROR_TAIL_LINES);
    let mut killed = false;
    loop {
        // A bounded wait rather than a blocking read: a build step can stay
        // silent for minutes, and a cancelled caller must not wait for its
        // next line to be noticed.
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok((is_stderr, line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                if let Some(report) = report {
                    report.line(&line);
                }
                if is_stderr {
                    if tail.len() == ERROR_TAIL_LINES {
                        tail.pop_front();
                    }
                    tail.push_back(line);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if !killed && report.is_some_and(|report| report.cancelled()) {
            killed = true;
            let _ = child.kill();
        }
    }
    for reader in readers {
        let _ = reader.join();
    }
    let status = child
        .wait()
        .with_context(|| format!("cannot wait for docker compose {action}"))?;
    if killed {
        return Err(anyhow::Error::new(progress::Cancelled));
    }
    if !status.success() {
        let tail: Vec<String> = tail.into_iter().collect();
        bail!("docker compose {action} failed: {}", tail.join("\n").trim());
    }
    Ok(())
}

/// The command line as a human reads it, for the `$ docker …` log line.
fn display_args(cmd: &Command) -> String {
    cmd.get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

static AVAILABLE: OnceLock<bool> = OnceLock::new();
static VERSION: OnceLock<Option<String>> = OnceLock::new();

/// Whether the `docker compose` CLI plugin is installed and can reach the
/// configured Engine socket. Probed once per process and cached. Any v2
/// release is accepted: the commands below are chosen to need nothing newer.
pub fn available(docker: &DockerConfig) -> bool {
    *AVAILABLE.get_or_init(|| probe(docker))
}

fn probe(docker: &DockerConfig) -> bool {
    Command::new("docker")
        .env("DOCKER_HOST", docker_host(docker))
        .arg("compose")
        .arg("version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// The plugin's version (`2.29.7`), cached; `None` when it cannot be asked.
/// Diagnostics only — nothing branches on it.
pub fn version(docker: &DockerConfig) -> Option<String> {
    VERSION
        .get_or_init(|| {
            let mut cmd = Command::new("docker");
            cmd.env("DOCKER_HOST", docker_host(docker))
                .args(["compose", "version", "--short"]);
            run(cmd, "version")
                .ok()
                .map(|out| out.trim().to_string())
                .filter(|v| !v.is_empty())
        })
        .clone()
}

/// Create the project's containers without starting them — the compose
/// equivalent of `docker_create` for a normal Docker app, so an install
/// provisions the same way regardless of runtime kind: the app exists on
/// disk (containers included) but stays stopped until `start()`.
pub fn create(docker: &DockerConfig, project: &ComposeProject) -> Result<()> {
    let mut cmd = command(docker, project);
    cmd.arg("create");
    run(cmd, "create").map(|_| ())
}

/// Idempotent start: also reconciles any drift from the compose file itself
/// (recreating a changed service), which is why a compose app has no
/// separate settings-drift recreate step the way a Docker app does.
pub fn up(docker: &DockerConfig, project: &ComposeProject) -> Result<()> {
    let mut cmd = command(docker, project);
    cmd.arg("up").arg("-d");
    run(cmd, "up").map(|_| ())
}

/// `up -d --no-deps <service>` — starts one service that has no container
/// at all yet (never created, or removed by hand), leaving the others alone.
pub fn up_service(docker: &DockerConfig, project: &ComposeProject, service: &str) -> Result<()> {
    run(up_service_command(docker, project, service), "up").map(|_| ())
}

fn up_service_command(docker: &DockerConfig, project: &ComposeProject, service: &str) -> Command {
    let mut cmd = command(docker, project);
    cmd.args(["up", "-d", "--no-deps", service]);
    cmd
}

/// Stops the project's containers — **not** `down`: every other runtime's
/// "stop" means "no longer running, still exists", and `down` would remove
/// the containers (and, with `-v`, the very volumes the app's data lives in).
pub fn stop(docker: &DockerConfig, project: &ComposeProject) -> Result<()> {
    let mut cmd = command(docker, project);
    cmd.arg("stop");
    run(cmd, "stop").map(|_| ())
}

/// Kills the project's containers (SIGKILL) — the forced
/// counterpart of [`stop`]; the containers stay, same as after a stop.
pub fn kill(docker: &DockerConfig, project: &ComposeProject) -> Result<()> {
    let mut cmd = command(docker, project);
    cmd.arg("kill");
    run(cmd, "kill").map(|_| ())
}

/// Tears the project down entirely (`asc app remove`): containers and
/// networks, `--remove-orphans` for services dropped from the compose file
/// since install. Named volumes are left alone, same as any other runtime.
/// Never used by an upgrade or a rebuild.
pub fn down(docker: &DockerConfig, project: &ComposeProject) -> Result<()> {
    let mut cmd = command(docker, project);
    cmd.arg("down").arg("--remove-orphans");
    run(cmd, "down").map(|_| ())
}

/// The project's logs, already multiplexed and prefixed by service — compose
/// does this multiplexing itself, unlike a single-container app.
pub fn logs(
    docker: &DockerConfig,
    project: &ComposeProject,
    tail: usize,
    timestamps: bool,
) -> Result<String> {
    run(
        logs_command(docker, project, None, tail, timestamps),
        "logs",
    )
}

/// The logs of one service across all its replicas, without the
/// `<service>-<n> |` prefix the whole-project log carries. A service with a
/// single container reads its log through bollard instead; this is the
/// multi-replica case, where compose's own multiplexing is what is wanted.
pub fn service_logs(
    docker: &DockerConfig,
    project: &ComposeProject,
    service: &str,
    tail: usize,
    timestamps: bool,
) -> Result<String> {
    run(
        logs_command(docker, project, Some(service), tail, timestamps),
        "logs",
    )
}

fn logs_command(
    docker: &DockerConfig,
    project: &ComposeProject,
    service: Option<&str>,
    tail: usize,
    timestamps: bool,
) -> Command {
    let mut cmd = command(docker, project);
    cmd.arg("logs")
        .arg("--no-color")
        .arg("--tail")
        .arg(tail.to_string());
    if timestamps {
        cmd.arg("--timestamps");
    }
    if let Some(service) = service {
        cmd.arg("--no-log-prefix").arg(service);
    }
    cmd
}

/// Follow-mode logs of the project (or of one `service`) for the console: an
/// initial `tail`, then live lines, cut at `since` so a restarted project
/// does not replay its earlier runs. The caller spawns it and kills it when
/// the client goes away.
pub fn logs_follow_command(
    docker: &DockerConfig,
    project: &ComposeProject,
    tail: usize,
    since: Option<time::OffsetDateTime>,
    service: Option<&str>,
) -> tokio::process::Command {
    let mut cmd = command(docker, project);
    cmd.args(["logs", "--no-color", "-f", "-n", &tail.to_string()]);
    if let Some(at) = since {
        // Seconds with nanoseconds: the plugin and the Engine both take a
        // fractional unix timestamp, so the cut is exact.
        cmd.arg("--since")
            .arg(format!("{}.{:09}", at.unix_timestamp(), at.nanosecond()));
    }
    if let Some(service) = service {
        cmd.arg("--no-log-prefix").arg(service);
    }
    tokio::process::Command::from(cmd)
}

/// Service names of the project (`config --services`), as the files —
/// profiles, `extends` and every `-f` given — define them.
pub fn config_services(docker: &DockerConfig, project: &ComposeProject) -> Result<Vec<String>> {
    let mut cmd = command(docker, project);
    cmd.args(["config", "--services"]);
    let out = run(cmd, "config")?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

/// Validate the project as given (`config -q`): every file parses, every
/// reference resolves. The daemon's override file is checked this way
/// before it replaces the live one.
pub fn config_validate(docker: &DockerConfig, project: &ComposeProject) -> Result<()> {
    let mut cmd = command(docker, project);
    cmd.args(["config", "-q"]);
    run(cmd, "config").map(|_| ())
}

/// The project as compose resolves it (`config`, YAML): `extends` followed,
/// `${…}` interpolated, every `-f` merged, paths made absolute. Parsed in
/// memory only — the output carries interpolated values (passwords from an
/// `.env` file), so it is never logged and the parsed model keeps no
/// environment values, only their names.
pub fn config_resolved(docker: &DockerConfig, project: &ComposeProject) -> Result<ResolvedProject> {
    let mut cmd = command(docker, project);
    cmd.arg("config");
    let out = run(cmd, "config")?;
    serde_yaml::from_str(&out).context("cannot parse docker compose config output")
}

/// `pull <service…>` — refresh the images of services that are not built
/// locally. Nothing to do (and no plugin call) for an empty list.
pub fn pull(
    docker: &DockerConfig,
    project: &ComposeProject,
    services: &[String],
    report: Option<&dyn InstallReporter>,
) -> Result<()> {
    if services.is_empty() {
        return Ok(());
    }
    run_streaming(pull_command(docker, project, services), "pull", report)
}

fn pull_command(docker: &DockerConfig, project: &ComposeProject, services: &[String]) -> Command {
    let mut cmd = command(docker, project);
    cmd.arg("pull").args(services);
    cmd
}

/// `build [--pull] <service…>` — rebuild the images of services with a
/// `build:` section; `pull` also refreshes their base images.
pub fn build(
    docker: &DockerConfig,
    project: &ComposeProject,
    services: &[String],
    pull: bool,
    report: Option<&dyn InstallReporter>,
) -> Result<()> {
    if services.is_empty() {
        return Ok(());
    }
    run_streaming(
        build_command(docker, project, services, pull),
        "build",
        report,
    )
}

fn build_command(
    docker: &DockerConfig,
    project: &ComposeProject,
    services: &[String],
    pull: bool,
) -> Command {
    let mut cmd = command(docker, project);
    cmd.arg("build");
    if pull {
        cmd.arg("--pull");
    }
    cmd.args(services);
    cmd
}

/// Force-recreate the project's containers from the current files and
/// images — the only moment an upgrade or a rebuild touches the running
/// stack. `running` keeps the stack's state: `up -d` for a running one, `up
/// --no-start` for a stopped one (created, not started). With `service`
/// only that service is recreated (`--no-deps`: its dependencies are left
/// running as they are); without it, `--remove-orphans` also drops the
/// containers of services the new files no longer define. Never `down`, and
/// never touches volumes.
pub fn up_recreate(
    docker: &DockerConfig,
    project: &ComposeProject,
    running: bool,
    service: Option<&str>,
    report: Option<&dyn InstallReporter>,
) -> Result<()> {
    run_streaming(
        up_recreate_command(docker, project, running, service),
        "up",
        report,
    )
}

fn up_recreate_command(
    docker: &DockerConfig,
    project: &ComposeProject,
    running: bool,
    service: Option<&str>,
) -> Command {
    let mut cmd = command(docker, project);
    cmd.arg("up");
    cmd.arg(if running { "-d" } else { "--no-start" });
    match service {
        Some(service) => {
            cmd.args(["--no-deps", "--force-recreate", service]);
        }
        None => {
            cmd.args(["--force-recreate", "--remove-orphans"]);
        }
    }
    cmd
}

/// One container of a compose project, as `docker compose ps` reports it.
#[derive(Debug, Clone)]
pub struct ComposeContainer {
    pub name: String,
    pub running: bool,
}

#[derive(Debug, Deserialize)]
struct PsEntry {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "State")]
    state: String,
}

/// `docker compose ps --all --format json`; a project with no containers at
/// all (never started) yields an empty list rather than an error.
pub fn ps(docker: &DockerConfig, project: &ComposeProject) -> Result<Vec<ComposeContainer>> {
    let mut cmd = command(docker, project);
    cmd.arg("ps").arg("--all").arg("--format").arg("json");
    parse_ps(&run(cmd, "ps")?)
}

/// Parse `ps --format json` output in both shapes compose v2 has printed:
/// one JSON array (early v2) or one JSON object per line (later releases).
fn parse_ps(out: &str) -> Result<Vec<ComposeContainer>> {
    let out = out.trim();
    let entries: Vec<PsEntry> = if out.starts_with('[') {
        serde_json::from_str(out).context("cannot parse docker compose ps output")?
    } else {
        out.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_str(line).context("cannot parse docker compose ps output"))
            .collect::<Result<_>>()?
    };
    Ok(entries
        .into_iter()
        .map(|entry| ComposeContainer {
            running: entry.state.eq_ignore_ascii_case("running"),
            name: entry.name,
        })
        .collect())
}

// ── The resolved project model (`config`) ─────────────────────────────

/// What the daemon reads out of a resolved `docker compose config`: just
/// enough to tell built services from pulled ones, check bind mounts and
/// report effective limits. Environment *values* are dropped while parsing.
#[derive(Debug, Default, Deserialize)]
pub struct ResolvedProject {
    #[serde(default)]
    pub services: BTreeMap<String, ResolvedService>,
    /// Top-level named volumes; a value may be `null`.
    #[serde(default)]
    pub volumes: BTreeMap<String, Option<ResolvedVolume>>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ResolvedService {
    #[serde(default)]
    pub image: Option<String>,
    /// Present when the service is built locally; its content is not needed.
    #[serde(default)]
    pub build: Option<serde_yaml::Value>,
    #[serde(default)]
    pub volumes: Vec<serde_yaml::Value>,
    /// Variable names only — see [`ResolvedProject`].
    #[serde(default, rename = "environment", deserialize_with = "env_keys")]
    pub environment_keys: Vec<String>,
    #[serde(default)]
    pub deploy: Option<ResolvedDeploy>,
    /// Legacy service-level limits (`cpus:`, `mem_limit:`).
    #[serde(default)]
    pub cpus: Option<serde_yaml::Value>,
    #[serde(default)]
    pub mem_limit: Option<serde_yaml::Value>,
    #[serde(default)]
    pub restart: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ResolvedDeploy {
    #[serde(default)]
    pub resources: Option<ResolvedResources>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ResolvedResources {
    #[serde(default)]
    pub limits: Option<ResolvedLimits>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ResolvedLimits {
    #[serde(default)]
    pub cpus: Option<serde_yaml::Value>,
    #[serde(default)]
    pub memory: Option<serde_yaml::Value>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ResolvedVolume {
    #[serde(default)]
    pub driver_opts: Option<BTreeMap<String, serde_yaml::Value>>,
}

/// Names of a service's `environment`, map or `KEY=value` list alike, with
/// every value dropped on the spot.
fn env_keys<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_yaml::Value>::deserialize(deserializer)?;
    let mut keys: Vec<String> = match value {
        Some(serde_yaml::Value::Mapping(map)) => map
            .into_iter()
            .filter_map(|(key, _)| key.as_str().map(str::to_string))
            .collect(),
        Some(serde_yaml::Value::Sequence(list)) => list
            .into_iter()
            .filter_map(|entry| {
                entry
                    .as_str()
                    .map(|e| e.split_once('=').map_or(e, |(k, _)| k).to_string())
            })
            .collect(),
        _ => Vec::new(),
    };
    keys.sort();
    keys.dedup();
    Ok(keys)
}

impl ResolvedService {
    /// Built from a `build:` section rather than pulled.
    pub fn buildable(&self) -> bool {
        self.build.is_some()
    }

    fn limits(&self) -> Option<&ResolvedLimits> {
        self.deploy
            .as_ref()
            .and_then(|d| d.resources.as_ref())
            .and_then(|r| r.limits.as_ref())
    }

    /// Effective CPU limit in cores, from `deploy.resources.limits.cpus` or
    /// the legacy `cpus:`.
    pub fn cpus_limit(&self) -> Option<f64> {
        self.limits()
            .and_then(|l| l.cpus.as_ref())
            .or(self.cpus.as_ref())
            .and_then(value_f64)
            .filter(|cpus| *cpus > 0.0)
    }

    /// Effective memory limit in bytes, from `deploy.resources.limits.memory`
    /// or the legacy `mem_limit:`.
    pub fn memory_limit(&self) -> Option<u64> {
        self.limits()
            .and_then(|l| l.memory.as_ref())
            .or(self.mem_limit.as_ref())
            .and_then(value_bytes)
            .filter(|bytes| *bytes > 0)
    }

    /// Whether the CPU limit is spelled with the legacy service-level key —
    /// an override must then use the same key, since compose refuses a
    /// service that sets the same limit both ways.
    pub fn legacy_cpus(&self) -> bool {
        self.cpus.is_some()
    }

    /// See [`Self::legacy_cpus`], for `mem_limit:`.
    pub fn legacy_memory(&self) -> bool {
        self.mem_limit.is_some()
    }

    /// Host sources of the service's bind mounts, absolute after resolution.
    pub fn bind_sources(&self) -> Vec<String> {
        self.volumes
            .iter()
            .filter_map(|volume| match volume {
                serde_yaml::Value::Mapping(map) => {
                    let kind = map
                        .get(serde_yaml::Value::String("type".into()))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let source = map
                        .get(serde_yaml::Value::String("source".into()))
                        .and_then(|v| v.as_str());
                    (kind == "bind")
                        .then_some(source)
                        .flatten()
                        .map(str::to_string)
                }
                // `config` always prints the long syntax; a short entry is
                // read the conservative way all the same.
                serde_yaml::Value::String(entry) => entry
                    .split_once(':')
                    .map(|(source, _)| source)
                    .filter(|source| source.starts_with(['.', '/', '~']))
                    .map(str::to_string),
                _ => None,
            })
            .collect()
    }
}

impl ResolvedProject {
    /// Every host path the project would mount: the bind mounts of every
    /// service plus the `device` of every local named volume declared with
    /// `o: bind` — the second way a compose file can map a host directory
    /// into a container.
    pub fn host_mount_sources(&self) -> Vec<String> {
        let mut sources: Vec<String> = self
            .services
            .values()
            .flat_map(ResolvedService::bind_sources)
            .collect();
        for volume in self.volumes.values().flatten() {
            let Some(opts) = &volume.driver_opts else {
                continue;
            };
            let binds = opts
                .get("o")
                .and_then(|o| o.as_str())
                .is_some_and(|o| o.split(',').any(|opt| opt.trim() == "bind"));
            if binds && let Some(device) = opts.get("device").and_then(|d| d.as_str()) {
                sources.push(device.to_string());
            }
        }
        sources
    }
}

/// A YAML number, or a number in a string (`"0.5"`).
fn value_f64(value: &serde_yaml::Value) -> Option<f64> {
    match value {
        serde_yaml::Value::Number(n) => n.as_f64(),
        serde_yaml::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// A byte count as compose resolves it (`"536870912"`), or the human form a
/// compose file may still carry (`512m`, `1g`, `256k`, `1024b`).
fn value_bytes(value: &serde_yaml::Value) -> Option<u64> {
    match value {
        serde_yaml::Value::Number(n) => n.as_u64(),
        serde_yaml::Value::String(s) => parse_memory(s),
        _ => None,
    }
}

/// `536870912`, `512m`, `512MB`, `1.5g` → bytes (binary units, as Docker
/// reads them).
pub fn parse_memory(raw: &str) -> Option<u64> {
    let raw = raw.trim().to_ascii_lowercase();
    let digits_end = raw
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(raw.len());
    let (number, unit) = raw.split_at(digits_end);
    let number: f64 = number.parse().ok()?;
    let factor: u64 = match unit.trim().trim_end_matches('b') {
        "" => 1,
        "k" => 1 << 10,
        "m" => 1 << 20,
        "g" => 1 << 30,
        "t" => 1 << 40,
        _ => return None,
    };
    let bytes = number * factor as f64;
    (bytes.is_finite() && bytes >= 0.0).then_some(bytes as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> ComposeProject {
        ComposeProject::new(
            "asc-demo",
            "/asc/apps/demo/repository",
            vec!["docker-compose.yml".into()],
        )
    }

    fn args(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn base() -> Vec<String> {
        [
            "compose",
            "--ansi",
            "never",
            "-p",
            "asc-demo",
            "-f",
            "docker-compose.yml",
        ]
        .map(str::to_string)
        .to_vec()
    }

    fn with_base(rest: &[&str]) -> Vec<String> {
        let mut all = base();
        all.extend(rest.iter().map(|s| s.to_string()));
        all
    }

    #[test]
    fn every_command_targets_the_project_from_its_own_directory() {
        let docker = DockerConfig::default();
        let cmd = command(&docker, &project());
        assert_eq!(cmd.get_program(), "docker");
        assert_eq!(
            cmd.get_current_dir(),
            Some(Path::new("/asc/apps/demo/repository"))
        );
        assert_eq!(args(&cmd), base());
        let host = cmd
            .get_envs()
            .find(|(key, _)| *key == "DOCKER_HOST")
            .and_then(|(_, value)| value)
            .map(|v| v.to_string_lossy().into_owned());
        assert_eq!(host, Some(format!("unix://{}", docker.socket.display())));
    }

    #[test]
    fn the_override_file_goes_last() {
        let docker = DockerConfig::default();
        let with = project().with_file(Path::new("/asc/apps/demo/.asc/compose.override.yaml"));
        let args = args(&command(&docker, &with));
        assert_eq!(
            args[args.len() - 2..],
            ["-f", "/asc/apps/demo/.asc/compose.override.yaml"]
        );
        assert_eq!(
            args[args.len() - 4..args.len() - 2],
            ["-f", "docker-compose.yml"]
        );
    }

    #[test]
    fn pull_and_build_name_their_services() {
        let docker = DockerConfig::default();
        let services = vec!["db".to_string(), "cache".to_string()];
        assert_eq!(
            args(&pull_command(&docker, &project(), &services)),
            with_base(&["pull", "db", "cache"])
        );
        let web = vec!["web".to_string()];
        assert_eq!(
            args(&build_command(&docker, &project(), &web, true)),
            with_base(&["build", "--pull", "web"])
        );
        assert_eq!(
            args(&build_command(&docker, &project(), &web, false)),
            with_base(&["build", "web"])
        );
    }

    #[test]
    fn recreate_keeps_the_stack_state_and_never_downs_it() {
        let docker = DockerConfig::default();
        assert_eq!(
            args(&up_recreate_command(&docker, &project(), true, None)),
            with_base(&["up", "-d", "--force-recreate", "--remove-orphans"])
        );
        assert_eq!(
            args(&up_recreate_command(&docker, &project(), false, None)),
            with_base(&["up", "--no-start", "--force-recreate", "--remove-orphans"])
        );
        assert_eq!(
            args(&up_recreate_command(&docker, &project(), true, Some("web"))),
            with_base(&["up", "-d", "--no-deps", "--force-recreate", "web"])
        );
        assert_eq!(
            args(&up_recreate_command(
                &docker,
                &project(),
                false,
                Some("web")
            )),
            with_base(&["up", "--no-start", "--no-deps", "--force-recreate", "web"])
        );
        for running in [true, false] {
            for service in [None, Some("web")] {
                let args = args(&up_recreate_command(&docker, &project(), running, service));
                assert!(
                    !args
                        .iter()
                        .any(|a| a == "down" || a == "-v" || a == "--volumes")
                );
            }
        }
    }

    #[test]
    fn a_missing_service_starts_alone() {
        let docker = DockerConfig::default();
        assert_eq!(
            args(&up_service_command(&docker, &project(), "worker")),
            with_base(&["up", "-d", "--no-deps", "worker"])
        );
    }

    #[test]
    fn service_logs_drop_the_prefix() {
        let docker = DockerConfig::default();
        assert_eq!(
            args(&logs_command(&docker, &project(), None, 50, false)),
            with_base(&["logs", "--no-color", "--tail", "50"])
        );
        assert_eq!(
            args(&logs_command(&docker, &project(), Some("web"), 50, true)),
            with_base(&[
                "logs",
                "--no-color",
                "--tail",
                "50",
                "--timestamps",
                "--no-log-prefix",
                "web"
            ])
        );
    }

    #[test]
    fn follow_logs_cut_at_the_run_start_with_nanoseconds() {
        let docker = DockerConfig::default();
        let cmd = logs_follow_command(&docker, &project(), 50, None, None);
        assert_eq!(
            args(cmd.as_std()),
            with_base(&["logs", "--no-color", "-f", "-n", "50"])
        );
        assert_eq!(
            cmd.as_std().get_current_dir(),
            Some(Path::new("/asc/apps/demo/repository"))
        );

        let at =
            time::OffsetDateTime::from_unix_timestamp_nanos(1_700_000_000_000_000_042).unwrap();
        let cmd = logs_follow_command(&docker, &project(), 50, Some(at), Some("db"));
        assert_eq!(
            args(cmd.as_std()),
            with_base(&[
                "logs",
                "--no-color",
                "-f",
                "-n",
                "50",
                "--since",
                "1700000000.000000042",
                "--no-log-prefix",
                "db"
            ])
        );
    }

    #[test]
    fn ps_accepts_an_array_and_ndjson() {
        let ndjson = "{\"Name\":\"asc-demo-web-1\",\"State\":\"running\"}\n\
                      {\"Name\":\"asc-demo-db-1\",\"State\":\"exited\"}\n";
        let array = "[{\"Name\":\"asc-demo-web-1\",\"State\":\"running\"},\
                     {\"Name\":\"asc-demo-db-1\",\"State\":\"exited\"}]";
        for out in [ndjson, array] {
            let containers = parse_ps(out).unwrap();
            assert_eq!(containers.len(), 2, "{out}");
            assert_eq!(containers[0].name, "asc-demo-web-1");
            assert!(containers[0].running);
            assert!(!containers[1].running);
        }
        // A project with no containers prints nothing (NDJSON) or `[]`.
        assert!(parse_ps("").unwrap().is_empty());
        assert!(parse_ps("[]\n").unwrap().is_empty());
        assert!(parse_ps("not json").is_err());
    }

    const RESOLVED: &str = r#"name: asc-demo
services:
  api:
    build:
      context: /asc/apps/demo/repository
      dockerfile: Dockerfile
    cpus: 1.5
    mem_limit: "268435456"
    environment:
      - TOKEN=abc
      - PLAIN
  web:
    deploy:
      resources:
        limits:
          cpus: 0.5
          memory: "536870912"
    environment:
      SECRET: hunter2
      FOO: bar
    image: nginx:1.27
    restart: unless-stopped
    volumes:
      - type: bind
        source: /asc/apps/demo/repository/data
        target: /data
        bind: {}
      - type: volume
        source: named
        target: /n
        volume: {}
volumes:
  named:
    name: asc-demo_named
  evil:
    driver_opts:
      type: none
      o: bind
      device: /etc
"#;

    #[test]
    fn the_resolved_model_reads_limits_builds_and_mounts() {
        let resolved: ResolvedProject = serde_yaml::from_str(RESOLVED).unwrap();
        let api = &resolved.services["api"];
        assert!(api.buildable());
        assert_eq!(api.cpus_limit(), Some(1.5));
        assert_eq!(api.memory_limit(), Some(268_435_456));
        assert!(api.legacy_cpus() && api.legacy_memory());
        assert_eq!(api.environment_keys, ["PLAIN", "TOKEN"]);

        let web = &resolved.services["web"];
        assert!(!web.buildable());
        assert_eq!(web.image.as_deref(), Some("nginx:1.27"));
        assert_eq!(web.cpus_limit(), Some(0.5));
        assert_eq!(web.memory_limit(), Some(536_870_912));
        assert!(!web.legacy_cpus() && !web.legacy_memory());
        assert_eq!(web.restart.as_deref(), Some("unless-stopped"));
        assert_eq!(web.environment_keys, ["FOO", "SECRET"]);

        let mut sources = resolved.host_mount_sources();
        sources.sort();
        assert_eq!(sources, ["/asc/apps/demo/repository/data", "/etc"]);
    }

    #[test]
    fn the_resolved_model_keeps_no_environment_values() {
        let resolved: ResolvedProject = serde_yaml::from_str(RESOLVED).unwrap();
        let debug = format!("{resolved:?}");
        assert!(!debug.contains("hunter2"), "{debug}");
        assert!(!debug.contains("abc"), "{debug}");
    }

    #[test]
    fn memory_sizes_parse_like_docker_reads_them() {
        assert_eq!(parse_memory("536870912"), Some(536_870_912));
        assert_eq!(parse_memory("512m"), Some(512 << 20));
        assert_eq!(parse_memory("512MB"), Some(512 << 20));
        assert_eq!(parse_memory("1g"), Some(1 << 30));
        assert_eq!(parse_memory("1.5g"), Some(3 << 29));
        assert_eq!(parse_memory("64k"), Some(64 << 10));
        assert_eq!(parse_memory("12x"), None);
        assert_eq!(parse_memory(""), None);
    }
}
