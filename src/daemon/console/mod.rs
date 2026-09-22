//! Console log sources for non-Docker runtimes (DMN-007).
//!
//! Docker containers stream through the Engine API (see
//! [`crate::daemon::docker`]); systemd units and plain processes stream from
//! a follow-mode subprocess produced here. The WebSocket transport lives in
//! `api::ws`; shared multi-client attach sessions live in [`hub`].

pub mod hub;

use std::path::Path;

use anyhow::{Result, bail};
use tokio::process::Command;

use crate::daemon::apps::meta::{AppMeta, Runtime};
use crate::daemon::config::DockerConfig;

/// Follow-mode log subprocess with an initial tail, for systemd/process/
/// compose apps. Docker apps do not use this — they stream over the Engine
/// API. `docker` is only used for a compose app, to point the plugin at the
/// same Engine socket every other command already does.
pub fn logs_command(
    meta: &AppMeta,
    dir: &Path,
    tail: usize,
    docker: &DockerConfig,
) -> Result<Command> {
    let tail = tail.to_string();
    match &meta.runtime {
        Runtime::Systemd { unit } => {
            let mut cmd = Command::new("journalctl");
            cmd.args([
                "-u",
                unit,
                "-f",
                "-n",
                &tail,
                "-o",
                "short-iso",
                "--no-pager",
            ]);
            Ok(cmd)
        }
        Runtime::Process { .. } => {
            let mut cmd = Command::new("tail");
            // -F: survive log rotation; the file may not exist yet.
            cmd.args(["-n", &tail, "-F"]).arg(dir.join("app.log"));
            Ok(cmd)
        }
        Runtime::Docker { .. } => {
            bail!("docker logs stream over the Engine API, not a subprocess")
        }
        // A compose project has no single container for exec/attach to
        // address (DMN-108), but its logs are already multiplexed and
        // service-prefixed by the plugin itself — the one console feature
        // that works exactly the same as any other runtime's live log.
        Runtime::Compose {
            project,
            files,
            working_dir,
        } => {
            let mut cmd = Command::new("docker");
            cmd.env("DOCKER_HOST", format!("unix://{}", docker.socket.display()));
            cmd.current_dir(dir.join(working_dir));
            cmd.arg("compose").arg("-p").arg(project);
            for file in files {
                cmd.arg("-f").arg(file);
            }
            cmd.args(["logs", "--no-color", "-f", "-n", &tail]);
            Ok(cmd)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::apps::meta::{DesiredState, Owner};

    fn meta(runtime: Runtime) -> AppMeta {
        AppMeta {
            id: "demo".into(),
            uuid: None,
            name: "demo".into(),
            custom_name: None,
            owner: Owner {
                uid: 0,
                name: "root".into(),
            },
            version: None,
            source: None,
            branch: None,
            repo_path: None,
            package: None,
            install_method: None,
            desired_state: DesiredState::Stopped,
            quota: None,
            runtime,
        }
    }

    #[test]
    fn subprocess_log_commands_per_runtime() {
        let dir = Path::new("/asc/apps/demo");
        let docker = DockerConfig::default();
        let systemd = logs_command(
            &meta(Runtime::Systemd {
                unit: "asc-app-demo.service".into(),
            }),
            dir,
            50,
            &docker,
        )
        .unwrap();
        assert_eq!(systemd.as_std().get_program(), "journalctl");

        let process = logs_command(
            &meta(Runtime::Process {
                command: "x".into(),
                args: vec![],
            }),
            dir,
            50,
            &docker,
        )
        .unwrap();
        assert_eq!(process.as_std().get_program(), "tail");
    }

    #[test]
    fn docker_has_no_subprocess_source() {
        let dir = Path::new("/asc/apps/demo");
        assert!(
            logs_command(
                &meta(Runtime::Docker {
                    container: "asc-demo".into(),
                    image_source: None,
                }),
                dir,
                50,
                &DockerConfig::default(),
            )
            .is_err()
        );
    }

    #[test]
    fn compose_follows_via_the_plugin_from_the_projects_own_directory() {
        let dir = Path::new("/asc/apps/demo");
        let docker = DockerConfig::default();
        let cmd = logs_command(
            &meta(Runtime::Compose {
                project: "asc-demo".into(),
                files: vec!["docker-compose.yml".into()],
                working_dir: "repository".into(),
            }),
            dir,
            50,
            &docker,
        )
        .unwrap();
        let std_cmd = cmd.as_std();
        assert_eq!(std_cmd.get_program(), "docker");
        assert_eq!(
            std_cmd.get_current_dir(),
            Some(Path::new("/asc/apps/demo/repository"))
        );
        let args: Vec<&str> = std_cmd.get_args().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            vec![
                "compose",
                "-p",
                "asc-demo",
                "-f",
                "docker-compose.yml",
                "logs",
                "--no-color",
                "-f",
                "-n",
                "50"
            ]
        );
    }
}
