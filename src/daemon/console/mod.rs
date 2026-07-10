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

/// Follow-mode log subprocess with an initial tail, for systemd/process apps.
/// Docker apps do not use this — they stream over the Engine API.
pub fn logs_command(meta: &AppMeta, dir: &Path, tail: usize) -> Result<Command> {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::apps::meta::{DesiredState, Owner};

    fn meta(runtime: Runtime) -> AppMeta {
        AppMeta {
            id: "demo".into(),
            name: "demo".into(),
            owner: Owner {
                uid: 0,
                name: "root".into(),
            },
            version: None,
            source: None,
            desired_state: DesiredState::Stopped,
            quota: None,
            runtime,
        }
    }

    #[test]
    fn subprocess_log_commands_per_runtime() {
        let dir = Path::new("/asc/apps/demo");
        let systemd = logs_command(
            &meta(Runtime::Systemd {
                unit: "asc-app-demo.service".into(),
            }),
            dir,
            50,
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
                    container: "asc-demo".into()
                }),
                dir,
                50
            )
            .is_err()
        );
    }
}
