//! `docker compose` orchestration (DMN-108/DMN-109) — the one deliberate
//! exception to "Docker only through bollard, never the CLI" (see
//! `AGENTS.md`/`CLAUDE.md`). A compose *project* — variable interpolation,
//! `env_file`, anchors, `extends`, profiles, several override files,
//! `depends_on` with healthcheck waits, per-service networks with DNS
//! aliases, `configs`/`secrets` — is something this daemon deliberately does
//! not model itself: the `docker compose` CLI plugin (v2) already does, so it
//! is shelled out to as an orchestrator, never modeled on top of the Engine
//! API the way ASC's own apps are.
//!
//! The plugin is optional: [`available`] probes it once at first use and
//! caches the answer (spawning a process on every capability check would be
//! wasteful, and the answer cannot change while the daemon is running). When
//! it is missing, `docker_compose` stays a **detected, not installable**
//! method — the same "found, not supported" shape Swarm/Kubernetes already
//! use — rather than the caller ever reaching the commands below.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::daemon::config::DockerConfig;

fn docker_host(docker: &DockerConfig) -> String {
    format!("unix://{}", docker.socket.display())
}

/// Base `docker compose -p <project> -f <file> ...` invocation, run from
/// `working_dir` (the compose file's own directory) so any relative paths
/// inside it — `build: .`, bind mounts, `env_file:` — resolve the way the
/// package author wrote them. `DOCKER_HOST` points the plugin at the
/// configured Engine socket, the same one bollard uses everywhere else in
/// this daemon, rather than whatever the ambient environment happens to have.
fn command(docker: &DockerConfig, project: &str, working_dir: &Path, files: &[String]) -> Command {
    let mut cmd = Command::new("docker");
    cmd.env("DOCKER_HOST", docker_host(docker));
    cmd.current_dir(working_dir);
    cmd.arg("compose").arg("-p").arg(project);
    for file in files {
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

static AVAILABLE: OnceLock<bool> = OnceLock::new();

/// Whether the `docker compose` CLI plugin is installed and can reach the
/// configured Engine socket. Probed once per process and cached.
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

/// Create the project's containers without starting them — the compose
/// equivalent of `docker_create` for a normal Docker app, so an install
/// provisions the same way regardless of runtime kind: the app exists on
/// disk (containers included) but stays stopped until `start()`.
pub fn create(
    docker: &DockerConfig,
    project: &str,
    working_dir: &Path,
    files: &[String],
) -> Result<()> {
    let mut cmd = command(docker, project, working_dir, files);
    cmd.arg("create");
    run(cmd, "create").map(|_| ())
}

/// Idempotent start: also reconciles any drift from the compose file itself
/// (recreating a changed service), which is why a compose app has no
/// separate settings-drift recreate step the way a Docker app does.
pub fn up(
    docker: &DockerConfig,
    project: &str,
    working_dir: &Path,
    files: &[String],
) -> Result<()> {
    let mut cmd = command(docker, project, working_dir, files);
    cmd.arg("up").arg("-d");
    run(cmd, "up").map(|_| ())
}

/// Stops the project's containers — **not** `down`: every other runtime's
/// "stop" means "no longer running, still exists", and `down` would remove
/// the containers (and, with `-v`, the very volumes the app's data lives in).
pub fn stop(
    docker: &DockerConfig,
    project: &str,
    working_dir: &Path,
    files: &[String],
) -> Result<()> {
    let mut cmd = command(docker, project, working_dir, files);
    cmd.arg("stop");
    run(cmd, "stop").map(|_| ())
}

/// Tears the project down entirely (`asc app remove`): containers and
/// networks, `--remove-orphans` for services dropped from the compose file
/// since install. Named volumes are left alone, same as any other runtime.
pub fn down(
    docker: &DockerConfig,
    project: &str,
    working_dir: &Path,
    files: &[String],
) -> Result<()> {
    let mut cmd = command(docker, project, working_dir, files);
    cmd.arg("down").arg("--remove-orphans");
    run(cmd, "down").map(|_| ())
}

/// The project's logs, already multiplexed and prefixed by service — compose
/// does this multiplexing itself, unlike a single-container app.
pub fn logs(
    docker: &DockerConfig,
    project: &str,
    working_dir: &Path,
    files: &[String],
    tail: usize,
    timestamps: bool,
) -> Result<String> {
    let mut cmd = command(docker, project, working_dir, files);
    cmd.arg("logs")
        .arg("--no-color")
        .arg("--tail")
        .arg(tail.to_string());
    if timestamps {
        cmd.arg("--timestamps");
    }
    run(cmd, "logs")
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

/// `docker compose ps --all --format json` — compose v2 prints one JSON
/// object **per line**, not a JSON array, so each line is parsed on its own;
/// a project with no containers at all (never started) yields an empty list
/// rather than an error.
pub fn ps(
    docker: &DockerConfig,
    project: &str,
    working_dir: &Path,
    files: &[String],
) -> Result<Vec<ComposeContainer>> {
    let mut cmd = command(docker, project, working_dir, files);
    cmd.arg("ps").arg("--all").arg("--format").arg("json");
    let out = run(cmd, "ps")?;
    let mut containers = Vec::new();
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let entry: PsEntry =
            serde_json::from_str(line).context("cannot parse docker compose ps output")?;
        containers.push(ComposeContainer {
            running: entry.state.eq_ignore_ascii_case("running"),
            name: entry.name,
        });
    }
    Ok(containers)
}
