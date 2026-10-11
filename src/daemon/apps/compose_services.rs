//! Services of a docker compose app: list, control, logs and per-service
//! settings.
//!
//! The split follows the daemon's bollard ↔ plugin boundary. Everything
//! that addresses existing containers goes through the Engine API: the
//! project's containers are found by the `com.docker.compose.project` /
//! `.service` labels compose writes on them, and start/stop/restart/kill,
//! one replica's logs, inspect and exec all act on those containers
//! directly. The plugin is only asked for what needs the project model:
//! creating a service that has no container yet (`up --no-deps`), the
//! multiplexed logs of several replicas, and `config` to validate and
//! resolve the settings ([`super::compose_override`]).
//!
//! None of this changes the app's desired state: stopping one service is
//! temporary, and the next start, rebuild or upgrade of the app brings it
//! back.

use std::path::Path;

use anyhow::Result;

use super::compose::{base_project_of, override_path, project_of};
use super::compose_override::{self, OverrideFile, ServiceOverride};
use super::meta::{AppMeta, Runtime};
use crate::daemon::compose;
use crate::daemon::config::Config;
use crate::daemon::docker::{self, ContainerInfo};
use crate::daemon::exec;
use crate::daemon::i18n::{Msg, tf, tf2};

/// The smallest memory limit the Engine accepts for a container.
pub const MIN_MEMORY_BYTES: u64 = 6 << 20;

/// Restart policies compose accepts (`on-failure` also as `on-failure:N`).
const RESTART_POLICIES: [&str; 4] = ["no", "always", "on-failure", "unless-stopped"];

/// One service in the app list: its name and its first container's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSummary {
    pub name: String,
    /// `running`, `exited`, `created`, `paused`, `restarting`, `dead`, or
    /// `missing` when the service has no container at all.
    pub state: String,
    /// `healthy`, `unhealthy`, `starting`, or empty without a healthcheck.
    pub health: String,
}

/// One service in detail.
#[derive(Debug)]
pub struct ServiceInfo {
    pub name: String,
    /// Name and id of the first replica's container; empty when missing.
    pub container: String,
    pub container_id: String,
    pub image: String,
    pub state: String,
    pub health: String,
    /// Engine status line ("Up 3 hours").
    pub status: String,
    pub exit_code: Option<i64>,
    pub started_at: Option<i64>,
    pub ports: Vec<docker::ContainerPortInfo>,
    pub replicas: u32,
    pub buildable: bool,
    pub has_overrides: bool,
}

/// What can be done to one service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceAction {
    Start,
    Stop,
    Restart,
    Kill,
}

impl ServiceAction {
    /// The REST path segment / CLI spelling.
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
            Self::Kill => "kill",
        }
    }

    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "start" => Some(Self::Start),
            "stop" => Some(Self::Stop),
            "restart" => Some(Self::Restart),
            "kill" => Some(Self::Kill),
            _ => None,
        }
    }
}

/// A service's settings: what the daemon's override file sets, and what
/// compose resolves with it applied. Environment *values* of the effective
/// configuration are never read — only the names.
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceSettings {
    pub overrides: ServiceOverride,
    pub effective_environment_keys: Vec<String>,
    pub effective_cpus: Option<f64>,
    pub effective_memory_bytes: Option<u64>,
    pub effective_restart: Option<String>,
}

/// Refuse anything but a compose app.
pub fn require_compose(meta: &AppMeta) -> Result<()> {
    match &meta.runtime {
        Runtime::Compose { .. } => Ok(()),
        other => Err(exec::invalid(tf2(
            Msg::AppServicesComposeOnly,
            &meta.id,
            other.kind(),
        ))),
    }
}

/// The project's service names: as `config --services` last recorded them
/// in the meta, or — for an app installed before that was recorded — read
/// from the compose files themselves.
pub fn service_names(meta: &AppMeta, app_dir: &Path) -> Vec<String> {
    let Runtime::Compose {
        files,
        working_dir,
        services,
        ..
    } = &meta.runtime
    else {
        return Vec::new();
    };
    if !services.is_empty() {
        let mut names = services.clone();
        names.sort();
        return names;
    }
    crate::daemon::pkg::compose::static_services(&app_dir.join(working_dir), files)
        .into_keys()
        .collect()
}

/// `NOT_FOUND` unless the project has `service`.
fn require_service(meta: &AppMeta, names: &[String], service: &str) -> Result<()> {
    if names.iter().any(|n| n == service) {
        Ok(())
    } else {
        Err(exec::not_found(tf2(
            Msg::AppServiceNotFound,
            service,
            &meta.id,
        )))
    }
}

/// A service's containers, the running ones first, then by name — the
/// first is "the" container a single-container view shows.
fn containers_of<'a>(containers: &'a [ContainerInfo], service: &str) -> Vec<&'a ContainerInfo> {
    let mut mine: Vec<&ContainerInfo> = containers
        .iter()
        .filter(|c| c.compose_service.as_deref() == Some(service))
        .collect();
    mine.sort_by(|a, b| {
        (a.state != "running")
            .cmp(&(b.state != "running"))
            .then_with(|| a.names.cmp(&b.names))
    });
    mine
}

/// Service summaries for the app list, from a container list the caller
/// already has (one Engine query for every app at once).
pub fn summaries(names: &[String], containers: &[ContainerInfo]) -> Vec<ServiceSummary> {
    names
        .iter()
        .map(|name| match containers_of(containers, name).first() {
            Some(first) => ServiceSummary {
                name: name.clone(),
                state: first.state.clone(),
                health: first.health.clone(),
            },
            None => ServiceSummary {
                name: name.clone(),
                state: "missing".to_string(),
                health: String::new(),
            },
        })
        .collect()
}

/// Every service of the app (or just `only`), sorted by name.
pub fn list(
    config: &Config,
    meta: &AppMeta,
    app_dir: &Path,
    only: Option<&str>,
) -> Result<Vec<ServiceInfo>> {
    require_compose(meta)?;
    let Runtime::Compose {
        project,
        files,
        working_dir,
        ..
    } = &meta.runtime
    else {
        unreachable!("checked above");
    };
    let names = service_names(meta, app_dir);
    if let Some(service) = only {
        require_service(meta, &names, service)?;
    }
    let statics = crate::daemon::pkg::compose::static_services(&app_dir.join(working_dir), files);
    let overrides = OverrideFile::load(&override_path(app_dir)).unwrap_or_default();
    let containers = docker::list_project_containers(&config.docker, project)?;
    let mut services = Vec::new();
    for name in names
        .iter()
        .filter(|n| only.is_none_or(|o| o == n.as_str()))
    {
        let mine = containers_of(&containers, name);
        let fixed = statics.get(name);
        let mut info = ServiceInfo {
            name: name.clone(),
            container: String::new(),
            container_id: String::new(),
            image: fixed.and_then(|s| s.image.clone()).unwrap_or_default(),
            state: "missing".to_string(),
            health: String::new(),
            status: String::new(),
            exit_code: None,
            started_at: None,
            ports: Vec::new(),
            replicas: mine.len() as u32,
            buildable: fixed.is_some_and(|s| s.buildable),
            has_overrides: overrides.services.contains_key(name),
        };
        if let Some(first) = mine.first() {
            let run = docker::container_run_info(&config.docker, &first.id)
                .ok()
                .flatten()
                .unwrap_or_default();
            info.container = first.names.first().cloned().unwrap_or_default();
            info.container_id = first.id.clone();
            info.image = first.image.clone();
            info.state = first.state.clone();
            info.health = first.health.clone();
            info.status = first.status.clone();
            info.exit_code = run.exit_code;
            info.started_at = run.started_at;
            info.ports = first.ports.clone();
        }
        services.push(info);
    }
    Ok(services)
}

/// Start, stop, restart or kill every container of one service. A service
/// with no container at all can only be started, by creating it
/// (`up -d --no-deps <service>`). Returns the service as it is afterwards.
pub fn control(
    config: &Config,
    meta: &AppMeta,
    app_dir: &Path,
    service: &str,
    action: ServiceAction,
) -> Result<ServiceInfo> {
    require_compose(meta)?;
    let names = service_names(meta, app_dir);
    require_service(meta, &names, service)?;
    let Runtime::Compose { project, .. } = &meta.runtime else {
        unreachable!("checked above");
    };
    let containers = docker::list_project_containers(&config.docker, project)?;
    let mine = containers_of(&containers, service);
    let cfg = &config.docker;
    match action {
        ServiceAction::Start if mine.is_empty() => {
            compose::up_service(cfg, &project_of(meta, app_dir)?, service)?;
        }
        ServiceAction::Start => {
            for container in &mine {
                docker::start(cfg, &container.id)?;
            }
        }
        ServiceAction::Stop => {
            for container in &mine {
                docker::stop(cfg, &container.id)?;
            }
        }
        ServiceAction::Restart => {
            for container in &mine {
                docker::restart(cfg, &container.id)?;
            }
        }
        ServiceAction::Kill => {
            for container in &mine {
                docker::kill(cfg, &container.id)?;
            }
        }
    }
    tracing::info!(app = %meta.id, service, action = ?action, "compose service controlled");
    list(config, meta, app_dir, Some(service))?
        .into_iter()
        .next()
        .ok_or_else(|| exec::not_found(tf2(Msg::AppServiceNotFound, service, &meta.id)))
}

/// The last `tail` log lines of one service, without the service-name
/// prefix. A single container is read through bollard; several replicas
/// are multiplexed by the plugin.
pub fn logs(
    config: &Config,
    meta: &AppMeta,
    app_dir: &Path,
    service: &str,
    tail: usize,
    timestamps: bool,
) -> Result<String> {
    require_compose(meta)?;
    let names = service_names(meta, app_dir);
    require_service(meta, &names, service)?;
    let Runtime::Compose { project, .. } = &meta.runtime else {
        unreachable!("checked above");
    };
    let containers = docker::list_project_containers(&config.docker, project)?;
    let mine = containers_of(&containers, service);
    match mine.as_slice() {
        [] => Ok(String::new()),
        [only] => docker::logs_tail(&config.docker, &only.id, tail, timestamps),
        _ => compose::service_logs(
            &config.docker,
            &project_of(meta, app_dir)?,
            service,
            tail,
            timestamps,
        ),
    }
}

/// Which container a console session for `service` addresses: its first
/// *running* replica (an exec needs a live process), else `None`.
pub fn running_container(config: &Config, meta: &AppMeta, service: &str) -> Result<Option<String>> {
    let Runtime::Compose { project, .. } = &meta.runtime else {
        return Ok(None);
    };
    let containers = docker::list_project_containers(&config.docker, project)?;
    Ok(containers_of(&containers, service)
        .into_iter()
        .find(|c| c.state == "running")
        .map(|c| c.id.clone()))
}

/// The containers of one service, for a console that streams their logs:
/// one id means bollard can follow it directly.
pub fn container_ids(config: &Config, meta: &AppMeta, service: &str) -> Result<Vec<String>> {
    let Runtime::Compose { project, .. } = &meta.runtime else {
        return Ok(Vec::new());
    };
    let containers = docker::list_project_containers(&config.docker, project)?;
    Ok(containers_of(&containers, service)
        .into_iter()
        .map(|c| c.id.clone())
        .collect())
}

/// `NOT_FOUND`/`INVALID_ARGUMENT` unless `service` is a service of the
/// compose app `meta` — the check a console token is issued behind.
pub fn require(meta: &AppMeta, app_dir: &Path, service: &str) -> Result<()> {
    require_compose(meta)?;
    require_service(meta, &service_names(meta, app_dir), service)
}

/// One service's settings.
pub fn settings(
    config: &Config,
    meta: &AppMeta,
    app_dir: &Path,
    service: &str,
) -> Result<ServiceSettings> {
    require(meta, app_dir, service)?;
    let overrides = OverrideFile::load(&override_path(app_dir))?
        .services
        .remove(service)
        .unwrap_or_default();
    let resolved = compose::config_resolved(&config.docker, &project_of(meta, app_dir)?)?;
    let effective = resolved.services.get(service);
    Ok(ServiceSettings {
        overrides,
        effective_environment_keys: effective
            .map(|s| s.environment_keys.clone())
            .unwrap_or_default(),
        effective_cpus: effective.and_then(|s| s.cpus_limit()),
        effective_memory_bytes: effective.and_then(|s| s.memory_limit()),
        effective_restart: effective.and_then(|s| s.restart.clone()),
    })
}

/// Check one service's overrides before anything is written.
pub fn validate(overrides: &ServiceOverride, host_cores: u32) -> Result<()> {
    for key in overrides.environment.keys() {
        let mut chars = key.chars();
        let valid = chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            return Err(exec::invalid(tf(Msg::AppServiceBadEnvName, key)));
        }
    }
    for value in overrides.environment.values() {
        if value.contains('\0') {
            return Err(exec::invalid(tf(Msg::AppServiceBadEnvName, "\\0")));
        }
    }
    if let Some(cpus) = overrides.cpus
        && !(cpus > 0.0 && cpus <= f64::from(host_cores))
    {
        return Err(exec::invalid(tf2(Msg::AppServiceBadCpus, cpus, host_cores)));
    }
    if let Some(memory) = overrides.memory_bytes
        && memory < MIN_MEMORY_BYTES
    {
        return Err(exec::invalid(tf(Msg::AppServiceBadMemory, memory)));
    }
    if let Some(restart) = &overrides.restart {
        let valid = RESTART_POLICIES.contains(&restart.as_str())
            || restart
                .strip_prefix("on-failure:")
                .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        if !valid {
            return Err(exec::invalid(tf(Msg::AppServiceBadRestart, restart)));
        }
    }
    Ok(())
}

/// What [`set_settings`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingsWrite {
    /// The running service was recreated with the new overrides.
    pub applied: bool,
    /// The service runs on the old configuration.
    pub restart_required: bool,
}

/// Replace one service's overrides (an empty [`ServiceOverride`] clears
/// them). The new file is written beside the live one, checked with
/// `config -q` and only then renamed over it. With `apply`, a running
/// service whose overrides changed is recreated on its own
/// (`up -d --no-deps --force-recreate`); a stopped one picks them up when it
/// next starts. The caller holds the app's operation lock.
pub fn set_settings(
    config: &Config,
    meta: &AppMeta,
    app_dir: &Path,
    service: &str,
    overrides: ServiceOverride,
    apply: bool,
) -> Result<SettingsWrite> {
    require(meta, app_dir, service)?;
    validate(&overrides, crate::daemon::pkg::resources::host_cores())?;
    let cfg = &config.docker;
    let path = override_path(app_dir);
    let mut file = OverrideFile::load(&path)?;
    let before = file.services.get(service).cloned().unwrap_or_default();
    if overrides.is_empty() {
        file.services.remove(service);
    } else {
        file.services.insert(service.to_string(), overrides.clone());
    }
    let changed = before != overrides;

    let base = base_project_of(meta, app_dir)?;
    let base_resolved = compose::config_resolved(cfg, &base)?;
    if file.services.is_empty() {
        compose::config_validate(cfg, &base)?;
        compose_override::store(&file, &base_resolved, &path)?;
    } else {
        let candidate = compose_override::write_candidate(&path, &file.render(&base_resolved))?;
        let checked = compose::config_validate(cfg, &base.with_file(&candidate))
            .and_then(|()| compose_override::commit(&candidate, &path));
        if let Err(err) = checked {
            let _ = std::fs::remove_file(&candidate);
            return Err(exec::invalid(format!("{err:#}")));
        }
    }
    record_services(config, meta, app_dir);

    let Runtime::Compose { project, .. } = &meta.runtime else {
        unreachable!("checked above");
    };
    let containers = docker::list_project_containers(cfg, project)?;
    let running = containers_of(&containers, service)
        .iter()
        .any(|c| c.state == "running");
    if running && changed && apply {
        compose::up_recreate(cfg, &project_of(meta, app_dir)?, true, Some(service), None)?;
        return Ok(SettingsWrite {
            applied: true,
            restart_required: false,
        });
    }
    Ok(SettingsWrite {
        applied: false,
        restart_required: running && changed,
    })
}

/// Refresh the recorded service names after a settings write — best
/// effort: the names only feed the service list.
fn record_services(config: &Config, meta: &AppMeta, app_dir: &Path) {
    let Ok(base) = base_project_of(meta, app_dir) else {
        return;
    };
    let Ok(names) = compose::config_services(&config.docker, &base) else {
        return;
    };
    let Ok(mut saved) = AppMeta::load(app_dir) else {
        return;
    };
    if let Runtime::Compose { services, .. } = &mut saved.runtime
        && *services != names
    {
        *services = names;
        let _ = saved.save(app_dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, HashMap};

    fn container(id: &str, name: &str, service: &str, state: &str, health: &str) -> ContainerInfo {
        ContainerInfo {
            id: id.into(),
            names: vec![name.into()],
            image: "img".into(),
            image_id: String::new(),
            state: state.into(),
            status: String::new(),
            created: 0,
            ports: Vec::new(),
            labels: HashMap::new(),
            compose_project: Some("asc-demo".into()),
            compose_service: Some(service.into()),
            health: health.into(),
            size_rw: None,
            size_root_fs: None,
            networks: Vec::new(),
            mounts: Vec::new(),
        }
    }

    #[test]
    fn a_service_without_a_container_is_missing() {
        let containers = vec![
            container("1", "asc-demo-web-1", "web", "running", "healthy"),
            container("2", "asc-demo-db-1", "db", "exited", ""),
        ];
        let names = ["db".to_string(), "web".to_string(), "worker".to_string()];
        let summaries = summaries(&names, &containers);
        assert_eq!(
            summaries,
            [
                ServiceSummary {
                    name: "db".into(),
                    state: "exited".into(),
                    health: String::new()
                },
                ServiceSummary {
                    name: "web".into(),
                    state: "running".into(),
                    health: "healthy".into()
                },
                ServiceSummary {
                    name: "worker".into(),
                    state: "missing".into(),
                    health: String::new()
                },
            ]
        );
    }

    #[test]
    fn the_first_replica_is_a_running_one() {
        let containers = vec![
            container("1", "asc-demo-web-1", "web", "exited", ""),
            container("2", "asc-demo-web-2", "web", "running", ""),
            container("3", "asc-demo-web-3", "web", "running", ""),
        ];
        let mine = containers_of(&containers, "web");
        assert_eq!(
            mine.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["2", "3", "1"]
        );
    }

    #[test]
    fn overrides_are_validated_field_by_field() {
        let ok = ServiceOverride {
            environment: BTreeMap::from([
                ("API_KEY".to_string(), "x".to_string()),
                ("_private2".to_string(), String::new()),
            ]),
            cpus: Some(0.5),
            memory_bytes: Some(MIN_MEMORY_BYTES),
            restart: Some("on-failure:5".into()),
        };
        validate(&ok, 4).unwrap();
        validate(&ServiceOverride::default(), 1).unwrap();

        let bad = |change: fn(&mut ServiceOverride)| {
            let mut o = ok.clone();
            change(&mut o);
            let err = validate(&o, 4).unwrap_err();
            assert!(
                matches!(
                    err.downcast_ref::<exec::ModuleError>(),
                    Some(exec::ModuleError::Invalid(_))
                ),
                "{err:#}"
            );
        };
        bad(|o| {
            o.environment.insert("1ST".into(), "x".into());
        });
        bad(|o| {
            o.environment.insert("WITH-DASH".into(), "x".into());
        });
        bad(|o| {
            o.environment.insert(String::new(), "x".into());
        });
        bad(|o| o.cpus = Some(0.0));
        bad(|o| o.cpus = Some(4.5));
        bad(|o| o.memory_bytes = Some(MIN_MEMORY_BYTES - 1));
        bad(|o| o.restart = Some("sometimes".into()));
        bad(|o| o.restart = Some("on-failure:".into()));
        bad(|o| o.restart = Some("on-failure:x".into()));
    }

    #[test]
    fn actions_parse_their_wire_names_only() {
        assert_eq!(ServiceAction::from_wire("kill"), Some(ServiceAction::Kill));
        assert_eq!(
            ServiceAction::from_wire("start"),
            Some(ServiceAction::Start)
        );
        assert_eq!(ServiceAction::from_wire("down"), None);
        assert_eq!(ServiceAction::from_wire("Stop"), None);
    }
}
