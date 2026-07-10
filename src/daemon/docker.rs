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
use bollard::container::{
    AttachContainerOptions, AttachContainerResults, Config as ContainerConfig,
    CreateContainerOptions, LogsOptions, RemoveContainerOptions, StartContainerOptions,
    StatsOptions, StopContainerOptions,
};
use bollard::errors::Error as BollardError;
use bollard::models::{HostConfig, PortBinding, RestartPolicy, RestartPolicyNameEnum};
use futures_util::{Stream, StreamExt};

use crate::daemon::config::DockerConfig;
use crate::daemon::i18n::{Msg, tf};

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

/// Map a Docker error to a user-facing one that names the socket path.
fn friendly(cfg: &DockerConfig, err: BollardError) -> anyhow::Error {
    anyhow!(
        "{}: {err}",
        tf(Msg::ErrDockerUnreachable, cfg.socket.display())
    )
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
            .start_container(container, None::<StartContainerOptions<String>>)
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
            t: STOP_TIMEOUT_SECS,
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
                let Some(memory) = stats.memory_stats.usage else {
                    return Ok(None);
                };
                // Engine reports CPU time in nanoseconds.
                let cpu_micros = stats.cpu_stats.cpu_usage.total_usage / 1_000;
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
        let opts = LogsOptions::<String> {
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
}

/// Create (but do not start) a container from a spec. Used by the installer.
pub fn create(cfg: &DockerConfig, spec: CreateSpec<'_>) -> Result<()> {
    block_on(async {
        let docker = connect(cfg)?;

        let mut exposed_ports: HashMap<String, HashMap<(), ()>> = HashMap::new();
        let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
        for port in &spec.ports {
            let key = format!("{port}/tcp");
            exposed_ports.insert(key.clone(), HashMap::new());
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

        let config = ContainerConfig {
            image: Some(spec.image.to_string()),
            env: (!spec.env.is_empty()).then(|| spec.env.clone()),
            exposed_ports: (!exposed_ports.is_empty()).then_some(exposed_ports),
            host_config: Some(host_config),
            ..Default::default()
        };

        docker
            .create_container(
                Some(CreateContainerOptions {
                    name: spec.name.to_string(),
                    platform: None,
                }),
                config,
            )
            .await
            .map_err(|e| friendly(cfg, e))?;
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
    let opts = LogsOptions::<String> {
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
    let opts = AttachContainerOptions::<String> {
        stdin: Some(true),
        stdout: Some(true),
        stderr: Some(true),
        stream: Some(true),
        logs: Some(false),
        detach_keys: None,
    };
    docker
        .attach_container(container, Some(opts))
        .await
        .map_err(|e| friendly(cfg, e))
}
