//! Docker Engine API client over the unix socket (configurable path).
//!
//! The daemon manages containers through the Engine API — **not** the
//! `docker` CLI — so rootless setups or a non-standard socket only need the
//! `[docker] socket` config. Control-plane operations are synchronous (the
//! app driver runs them via [`block_on`]); the console uses the async
//! streaming helpers directly on the API runtime.

use std::collections::HashMap;
use std::future::Future;

use anyhow::{Result, anyhow};
use bollard::Docker;
use bollard::container::AttachContainerResults;
use bollard::errors::Error as BollardError;
use bollard::models::{
    ContainerCreateBody, HostConfig, PortBinding, RestartPolicy, RestartPolicyNameEnum,
};
use bollard::query_parameters::{
    AttachContainerOptions, CreateContainerOptions, CreateImageOptions, LogsOptions,
    RemoveContainerOptions, StartContainerOptions, StatsOptions, StopContainerOptions,
};
use futures_util::{Stream, StreamExt};
use tracing::info;

use crate::daemon::config::DockerConfig;
use crate::daemon::i18n::{Msg, t, tf};

/// Seconds the Engine waits on stop before killing the container.
const STOP_TIMEOUT_SECS: i64 = 10;
/// Client connect/request timeout, seconds.
const CONNECT_TIMEOUT_SECS: u64 = 120;

/// Connect to the Docker Engine over the configured unix socket.
///
/// Connection is lazy (bollard connects on first request), so this only
/// fails fast when the socket file is missing; live errors surface per call.
pub fn connect(cfg: &DockerConfig) -> Result<Docker> {
    let socket = cfg.socket.to_string_lossy();
    Docker::connect_with_unix(&socket, CONNECT_TIMEOUT_SECS, bollard::API_DEFAULT_VERSION)
        .map_err(|err| friendly(cfg, err))
}

/// Map a Docker error to a user-facing one. An Engine response (any HTTP
/// status) proves Docker is reachable — pass its own message through instead
/// of blaming the socket. A host without the docker binary has Docker
/// missing, not stopped — say that and how to install it instead of asking
/// whether the daemon is running.
fn friendly(cfg: &DockerConfig, err: BollardError) -> anyhow::Error {
    if status_of(&err).is_some() {
        return anyhow!("{err}");
    }
    if !docker_binary_present() {
        return anyhow!("{}: {err}", t(Msg::ErrDockerNotFound));
    }
    anyhow!(
        "{}: {err}",
        tf(Msg::ErrDockerUnreachable, cfg.socket.display())
    )
}

/// Whether a `docker` executable is anywhere on PATH.
fn docker_binary_present() -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join("docker").is_file()))
        .unwrap_or(false)
}

/// HTTP status carried by a Docker Engine error response, if any.
fn status_of(err: &BollardError) -> Option<u16> {
    match err {
        BollardError::DockerResponseServerError { status_code, .. } => Some(*status_code),
        _ => None,
    }
}

/// Run a future to completion on a fresh current-thread runtime.
///
/// Driver operations are infrequent control-plane calls and never run inside
/// an ambient async context (the CLI is synchronous; the API wraps driver
/// calls in `spawn_blocking`), so a throwaway runtime per call is safe.
pub fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("cannot build docker runtime")
        .block_on(future)
}

// ── Synchronous control-plane operations (app driver) ───────────────────────

/// Start a container. A 304 (already started) is treated as success.
pub fn start(cfg: &DockerConfig, container: &str) -> Result<()> {
    block_on(async {
        let docker = connect(cfg)?;
        match docker
            .start_container(container, None::<StartContainerOptions>)
            .await
        {
            Ok(()) => Ok(()),
            Err(e) if status_of(&e) == Some(304) => Ok(()),
            Err(e) => Err(friendly(cfg, e)),
        }
    })
}

/// Stop a container (graceful, then kill after the timeout). 304 = already stopped.
pub fn stop(cfg: &DockerConfig, container: &str) -> Result<()> {
    block_on(async {
        let docker = connect(cfg)?;
        let opts = StopContainerOptions {
            t: Some(STOP_TIMEOUT_SECS as i32),
            ..Default::default()
        };
        match docker.stop_container(container, Some(opts)).await {
            Ok(()) => Ok(()),
            Err(e) if status_of(&e) == Some(304) => Ok(()),
            Err(e) => Err(friendly(cfg, e)),
        }
    })
}

pub fn restart(cfg: &DockerConfig, container: &str) -> Result<()> {
    block_on(async {
        let docker = connect(cfg)?;
        docker
            .restart_container(container, None)
            .await
            .map_err(|e| friendly(cfg, e))
    })
}

/// Whether the container exists and is running. A missing container (404)
/// reads as not running.
pub fn running(cfg: &DockerConfig, container: &str) -> Result<bool> {
    block_on(async {
        let docker = connect(cfg)?;
        match docker.inspect_container(container, None).await {
            Ok(info) => Ok(info.state.and_then(|s| s.running).unwrap_or(false)),
            Err(e) if status_of(&e) == Some(404) => Ok(false),
            Err(e) => Err(friendly(cfg, e)),
        }
    })
}

/// The parts of a container's configuration the daemon manages, read back
/// from inspect for settings-drift detection (see `pkg::refresh`).
#[derive(Debug)]
pub struct AppliedConfig {
    /// `Config.Env` — includes the image's own variables.
    pub env: Vec<String>,
    /// `HostConfig.Binds`, sorted.
    pub binds: Vec<String>,
    /// Published port keys (`"27015/tcp"`), sorted.
    pub ports: Vec<String>,
    /// `HostConfig.NanoCpus`; 0 = unlimited.
    pub nano_cpus: i64,
    /// `HostConfig.Memory`, bytes; 0 = unlimited.
    pub memory: i64,
    /// `Config.Cmd` — a `start_command` override lands here.
    pub cmd: Option<Vec<String>>,
}

/// Inspect the daemon-managed configuration of a container. `None` when the
/// container does not exist (404).
pub fn container_applied(cfg: &DockerConfig, container: &str) -> Result<Option<AppliedConfig>> {
    block_on(async {
        let docker = connect(cfg)?;
        match docker.inspect_container(container, None).await {
            Ok(info) => {
                let config = info.config.unwrap_or_default();
                let host = info.host_config.unwrap_or_default();
                let mut ports: Vec<String> = host
                    .port_bindings
                    .map(|map| map.into_keys().collect())
                    .unwrap_or_default();
                ports.sort();
                let mut binds = host.binds.unwrap_or_default();
                binds.sort();
                Ok(Some(AppliedConfig {
                    env: config.env.unwrap_or_default(),
                    binds,
                    ports,
                    nano_cpus: host.nano_cpus.unwrap_or(0),
                    memory: host.memory.unwrap_or(0),
                    cmd: config.cmd,
                }))
            }
            Err(e) if status_of(&e) == Some(404) => Ok(None),
            Err(e) => Err(friendly(cfg, e)),
        }
    })
}

/// Force-remove the container. A missing container (404) is success.
pub fn remove(cfg: &DockerConfig, container: &str) -> Result<()> {
    block_on(async {
        let docker = connect(cfg)?;
        let opts = RemoveContainerOptions {
            force: true,
            ..Default::default()
        };
        match docker.remove_container(container, Some(opts)).await {
            Ok(()) => Ok(()),
            Err(e) if status_of(&e) == Some(404) => Ok(()),
            Err(e) => Err(friendly(cfg, e)),
        }
    })
}

/// One-shot resource counters of a container: cumulative CPU time in
/// microseconds and resident memory in bytes. `None` when the container is
/// missing (404) or the Engine reports no memory usage (not running).
pub fn stats_usage(cfg: &DockerConfig, container: &str) -> Result<Option<(u64, u64)>> {
    block_on(async {
        let docker = connect(cfg)?;
        let opts = StatsOptions {
            stream: false,
            one_shot: true,
        };
        let mut stream = docker.stats(container, Some(opts));
        match stream.next().await {
            Some(Ok(stats)) => {
                let Some(memory) = stats.memory_stats.and_then(|m| m.usage) else {
                    return Ok(None);
                };
                // Engine reports CPU time in nanoseconds.
                let Some(cpu_micros) = stats
                    .cpu_stats
                    .and_then(|c| c.cpu_usage)
                    .and_then(|u| u.total_usage)
                    .map(|n| n / 1_000)
                else {
                    return Ok(None);
                };
                Ok(Some((cpu_micros, memory)))
            }
            Some(Err(e)) if status_of(&e) == Some(404) => Ok(None),
            Some(Err(e)) => Err(friendly(cfg, e)),
            None => Ok(None),
        }
    })
}

/// Last `tail` lines of the container's logs (non-follow), stdout+stderr.
pub fn logs_tail(cfg: &DockerConfig, container: &str, tail: usize) -> Result<String> {
    block_on(async {
        let docker = connect(cfg)?;
        let opts = LogsOptions {
            stdout: true,
            stderr: true,
            follow: false,
            tail: tail.to_string(),
            ..Default::default()
        };
        let mut stream = docker.logs(container, Some(opts));
        let mut out = String::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(log) => out.push_str(&String::from_utf8_lossy(&log.into_bytes())),
                // Container not created yet / removed: no logs, not an error.
                Err(e) if status_of(&e) == Some(404) => return Ok(String::new()),
                Err(e) => return Err(friendly(cfg, e)),
            }
        }
        Ok(out)
    })
}

/// Container definition for [`create`].
pub struct CreateSpec<'a> {
    pub name: &'a str,
    pub image: &'a str,
    /// Environment entries as `KEY=value`.
    pub env: Vec<String>,
    /// Ports to publish (host==container).
    pub ports: Vec<u16>,
    /// Volume binds as `host_path:container_path`.
    pub binds: Vec<String>,
    /// CPU quota in units of 1e-9 cores (Engine `NanoCpus`); `None` = unlimited.
    pub nano_cpus: Option<i64>,
    /// Memory limit in bytes (Engine `Memory`); `None` = unlimited.
    pub memory_bytes: Option<i64>,
    /// Start command override (`start_command` from asc.settings.yaml):
    /// replaces the image entrypoint, runs through `/bin/sh -c`.
    pub command: Option<String>,
    /// Keep the container's stdin open (Engine `OpenStdin`, like `docker run
    /// -i`) so attach input reaches the app.
    pub open_stdin: bool,
    /// Allocate a pseudo-TTY (Engine `Tty`, like `docker run -t`).
    pub tty: bool,
}

/// Split an image reference into the `fromImage` and `tag` query parameters
/// of the Engine pull endpoint. A bare name gets an explicit `latest` — an
/// empty tag makes the Engine pull every tag of the repository. Digest
/// references go through whole: the Engine pulls by digest, no tag needed.
fn image_ref(image: &str) -> (&str, Option<&str>) {
    if image.contains('@') {
        return (image, None);
    }
    // A colon is the tag separator only after the last slash; earlier it is
    // a registry port (localhost:5000/app).
    let name_start = image.rfind('/').map_or(0, |i| i + 1);
    match image[name_start..].rfind(':') {
        Some(i) => (&image[..name_start + i], Some(&image[name_start + i + 1..])),
        None => (image, Some("latest")),
    }
}

/// Pull an image from its registry, waiting until the Engine finishes.
async fn pull(docker: &Docker, image: &str) -> std::result::Result<(), BollardError> {
    let (from_image, tag) = image_ref(image);
    let opts = CreateImageOptions {
        from_image: Some(from_image.to_string()),
        tag: tag.map(str::to_string),
        ..Default::default()
    };
    let mut progress = docker.create_image(Some(opts), None, None);
    while let Some(step) = progress.next().await {
        step?;
    }
    Ok(())
}

/// Create (but do not start) a container from a spec. Used by the installer.
/// An image missing on the host is pulled from its registry automatically.
pub fn create(cfg: &DockerConfig, spec: CreateSpec<'_>) -> Result<()> {
    block_on(async {
        let docker = connect(cfg)?;

        let mut exposed_ports: Vec<String> = Vec::new();
        let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
        for port in &spec.ports {
            let key = format!("{port}/tcp");
            exposed_ports.push(key.clone());
            port_bindings.insert(
                key,
                Some(vec![PortBinding {
                    host_ip: None,
                    host_port: Some(port.to_string()),
                }]),
            );
        }

        let host_config = HostConfig {
            port_bindings: (!port_bindings.is_empty()).then_some(port_bindings),
            binds: (!spec.binds.is_empty()).then_some(spec.binds.clone()),
            restart_policy: Some(RestartPolicy {
                name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                maximum_retry_count: None,
            }),
            nano_cpus: spec.nano_cpus,
            memory: spec.memory_bytes,
            ..Default::default()
        };

        let config = ContainerCreateBody {
            image: Some(spec.image.to_string()),
            // A start_command replaces whatever the image would run: the
            // entrypoint becomes the shell so the command can use arguments
            // and env references.
            entrypoint: spec
                .command
                .as_ref()
                .map(|_| vec!["/bin/sh".to_string(), "-c".to_string()]),
            cmd: spec.command.as_ref().map(|c| vec![c.clone()]),
            env: (!spec.env.is_empty()).then(|| spec.env.clone()),
            open_stdin: spec.open_stdin.then_some(true),
            tty: spec.tty.then_some(true),
            exposed_ports: (!exposed_ports.is_empty()).then_some(exposed_ports),
            host_config: Some(host_config),
            ..Default::default()
        };

        let options = CreateContainerOptions {
            name: Some(spec.name.to_string()),
            ..Default::default()
        };
        match docker
            .create_container(Some(options.clone()), config.clone())
            .await
        {
            Ok(_) => {}
            // 404 = the image is not on the host: pull it and retry once.
            Err(e) if status_of(&e) == Some(404) => {
                info!(image = spec.image, "image not found locally, pulling");
                pull(&docker, spec.image)
                    .await
                    .map_err(|e| anyhow!("{}: {e}", tf(Msg::ErrImagePull, spec.image)))?;
                docker
                    .create_container(Some(options), config)
                    .await
                    .map_err(|e| friendly(cfg, e))?;
            }
            Err(e) => return Err(friendly(cfg, e)),
        }
        Ok(())
    })
}

// ── Async streaming operations (WebSocket console) ──────────────────────────

/// Follow-mode logs as a stream of UTF-8 text lines (trailing newline
/// stripped). Timestamps are included by the Engine.
pub async fn logs_follow(
    cfg: &DockerConfig,
    container: &str,
    tail: usize,
) -> Result<impl Stream<Item = Result<String>> + Send> {
    let docker = connect(cfg)?;
    let opts = LogsOptions {
        follow: true,
        stdout: true,
        stderr: true,
        timestamps: true,
        tail: tail.to_string(),
        ..Default::default()
    };
    // The stream owns its transport handle, so `docker` may drop here.
    let stream = docker.logs(container, Some(opts)).map(|item| {
        item.map(|log| {
            let mut line = String::from_utf8_lossy(&log.into_bytes()).into_owned();
            while line.ends_with('\n') || line.ends_with('\r') {
                line.pop();
            }
            line
        })
        .map_err(|e| anyhow!("docker logs: {e}"))
    });
    Ok(stream)
}

/// Interactive attach: bidirectional stdin/stdout to a running container.
pub async fn attach(cfg: &DockerConfig, container: &str) -> Result<AttachContainerResults> {
    let docker = connect(cfg)?;
    let opts = AttachContainerOptions {
        stdin: true,
        stdout: true,
        stderr: true,
        stream: true,
        logs: false,
        detach_keys: None,
    };
    docker
        .attach_container(container, Some(opts))
        .await
        .map_err(|e| friendly(cfg, e))
}

#[cfg(test)]
mod tests {
    use super::image_ref;

    #[test]
    fn image_refs_split_into_name_and_tag() {
        assert_eq!(image_ref("nginx"), ("nginx", Some("latest")));
        assert_eq!(image_ref("nginx:1.27"), ("nginx", Some("1.27")));
        assert_eq!(
            image_ref("steamcmd/steamcmd:latest"),
            ("steamcmd/steamcmd", Some("latest"))
        );
        assert_eq!(
            image_ref("localhost:5000/app"),
            ("localhost:5000/app", Some("latest"))
        );
        assert_eq!(
            image_ref("localhost:5000/app:v2"),
            ("localhost:5000/app", Some("v2"))
        );
        assert_eq!(image_ref("redis@sha256:abc"), ("redis@sha256:abc", None));
    }
}
