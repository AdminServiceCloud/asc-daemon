//! Console log sources for non-Docker runtimes (DMN-007).
//!
//! Docker containers stream through the Engine API (see
//! [`crate::daemon::docker`]); systemd units and plain processes stream from
//! a follow-mode subprocess produced here. The WebSocket transport lives in
//! `api::ws`; shared multi-client attach sessions live in [`hub`].

pub mod hub;

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Result, bail};
use tokio::process::Command;

use crate::daemon::apps::meta::{AppMeta, Runtime};
use crate::daemon::apps::process;
use crate::daemon::config::DockerConfig;
use crate::daemon::{compose, docker};

/// How far back from the end of `app.log` a process app's backlog is looked
/// for: bounds the read when the current run has written a lot.
const PROCESS_BACKLOG_WINDOW_BYTES: u64 = 256 * 1024;

/// Where the app's current run begins in its log source (DMN-116). Every
/// runtime here appends each run to one shared log — the journal, `app.log`,
/// the containers' Engine logs — so without this the console's initial tail
/// reaches back into earlier runs, and a restart shows the old output again.
#[derive(Debug, Clone, PartialEq)]
pub enum RunStart {
    /// Never started, or the source cannot tell: the plain tail.
    Unknown,
    /// systemd: the unit's current `InvocationID` — every journal line the
    /// run logs carries it, so the cut is exact.
    Invocation(String),
    /// process: the byte in `app.log` to start following from — already
    /// moved forward to the last `tail` lines of the current run.
    Offset(u64),
    /// compose: when the project's earliest running container started.
    Since(time::OffsetDateTime),
}

/// Resolves the app's [`RunStart`]. Never fails: a source that cannot be
/// asked degrades to [`RunStart::Unknown`], i.e. the old behaviour, rather
/// than refusing the console.
pub async fn run_start(meta: &AppMeta, dir: &Path, tail: usize, docker: &DockerConfig) -> RunStart {
    match &meta.runtime {
        Runtime::Systemd { unit } => systemd_invocation(unit)
            .await
            .map_or(RunStart::Unknown, RunStart::Invocation),
        Runtime::Process { .. } => {
            let dir = dir.to_path_buf();
            tokio::task::spawn_blocking(move || process_follow_start(&dir, tail))
                .await
                .ok()
                .flatten()
                .map_or(RunStart::Unknown, RunStart::Offset)
        }
        Runtime::Compose {
            project,
            files,
            working_dir,
        } => compose_started_at(docker, project, &dir.join(working_dir), files)
            .await
            .map_or(RunStart::Unknown, RunStart::Since),
        Runtime::Docker { .. } => RunStart::Unknown,
    }
}

/// The unit's current `InvocationID`; `None` while it is inactive (systemd
/// reports an empty value) or when `systemctl` cannot be asked.
async fn systemd_invocation(unit: &str) -> Option<String> {
    let out = Command::new("systemctl")
        .args(["show", "-p", "InvocationID", "--value", unit])
        .output()
        .await
        .ok()?;
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !id.is_empty()).then_some(id)
}

/// Where to start following a process app's `app.log`, from the run offset
/// its driver records on start. `None` when there is no marker (the process
/// was started by a daemon that predates it).
fn process_follow_start(dir: &Path, tail: usize) -> Option<u64> {
    let raw = std::fs::read_to_string(dir.join(process::RUN_OFFSET_FILE)).ok()?;
    let run_offset: u64 = raw.trim().parse().ok()?;
    match File::open(dir.join(process::LOG_FILE)) {
        Ok(mut log) => backlog_start(&mut log, run_offset, tail).ok(),
        // Not written yet: follow from its first byte once it appears.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(0),
        Err(_) => None,
    }
}

/// Byte position of the first of the last `tail` lines written at or after
/// `run_offset`, reading at most [`PROCESS_BACKLOG_WINDOW_BYTES`] from the
/// end. A file shorter than `run_offset` was truncated or rotated after the
/// run began, so all of it is the current run's.
fn backlog_start<F: Read + Seek>(
    log: &mut F,
    run_offset: u64,
    tail: usize,
) -> std::io::Result<u64> {
    let len = log.seek(SeekFrom::End(0))?;
    if tail == 0 {
        return Ok(len);
    }
    let run_offset = if run_offset > len { 0 } else { run_offset };
    let window_start = run_offset.max(len.saturating_sub(PROCESS_BACKLOG_WINDOW_BYTES));
    log.seek(SeekFrom::Start(window_start))?;
    let mut window = Vec::new();
    log.take(len - window_start).read_to_end(&mut window)?;

    // A trailing newline ends the last line rather than opening a new one.
    let body = window.strip_suffix(b"\n").unwrap_or(&window);
    let mut seen = 0;
    for (at, _) in body.iter().enumerate().rev().filter(|(_, b)| **b == b'\n') {
        seen += 1;
        if seen == tail {
            return Ok(window_start + at as u64 + 1);
        }
    }
    // Fewer than `tail` lines in the window. If the window was cut short of
    // the run's start, its first line is a fragment: skip it.
    if window_start > run_offset {
        let first_end = window.iter().position(|&b| b == b'\n');
        return Ok(window_start + first_end.map_or(window.len(), |at| at + 1) as u64);
    }
    Ok(window_start)
}

/// When the compose project's earliest running container started; `None`
/// when nothing runs (the plain tail then) or the plugin cannot be asked.
async fn compose_started_at(
    docker_cfg: &DockerConfig,
    project: &str,
    working_dir: &Path,
    files: &[String],
) -> Option<time::OffsetDateTime> {
    let (cfg, project, working_dir, files) = (
        docker_cfg.clone(),
        project.to_string(),
        working_dir.to_path_buf(),
        files.to_vec(),
    );
    let containers =
        tokio::task::spawn_blocking(move || compose::ps(&cfg, &project, &working_dir, &files))
            .await
            .ok()?
            .ok()?;
    let mut earliest = None;
    for container in containers.iter().filter(|c| c.running) {
        if let Ok(Some(started)) = docker::run_started_at(docker_cfg, &container.name).await {
            earliest = Some(earliest.map_or(started, |at: time::OffsetDateTime| at.min(started)));
        }
    }
    earliest
}

/// Follow-mode log subprocess with an initial tail, for systemd/process/
/// compose apps, limited to the current run — see [`run_start`]. Docker apps
/// do not use this — they stream over the Engine API. `docker` is only used
/// for a compose app, to point the plugin at the same Engine socket every
/// other command already does.
pub async fn logs_command(
    meta: &AppMeta,
    dir: &Path,
    tail: usize,
    docker: &DockerConfig,
) -> Result<Command> {
    let run = run_start(meta, dir, tail, docker).await;
    build_logs_command(meta, dir, tail, docker, &run)
}

fn build_logs_command(
    meta: &AppMeta,
    dir: &Path,
    tail: usize,
    docker: &DockerConfig,
    run: &RunStart,
) -> Result<Command> {
    let tail = tail.to_string();
    match &meta.runtime {
        Runtime::Systemd { unit } => {
            let mut cmd = Command::new("journalctl");
            match run {
                // The invocation match alone: `-u` expands into a
                // disjunction of several matches, and one more field ANDed
                // onto it would bind to its last term only.
                RunStart::Invocation(id) => cmd.arg(format!("_SYSTEMD_INVOCATION_ID={id}")),
                _ => cmd.args(["-u", unit]),
            };
            cmd.args(["-f", "-n", &tail, "-o", "short-iso", "--no-pager"]);
            Ok(cmd)
        }
        Runtime::Process { .. } => {
            let mut cmd = Command::new("tail");
            // -F: survive log rotation; the file may not exist yet.
            match run {
                RunStart::Offset(at) => cmd.args(["-c", &format!("+{}", at + 1), "-F"]),
                _ => cmd.args(["-n", &tail, "-F"]),
            };
            cmd.arg(dir.join(process::LOG_FILE));
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
            if let RunStart::Since(at) = run {
                // Seconds with nanoseconds: the plugin and the Engine both
                // take a fractional unix timestamp, so the cut is exact.
                cmd.arg("--since")
                    .arg(format!("{}.{:09}", at.unix_timestamp(), at.nanosecond()));
            }
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

    fn args(cmd: &Command) -> Vec<String> {
        cmd.as_std()
            .get_args()
            .map(|a| a.to_str().unwrap().to_string())
            .collect()
    }

    fn systemd() -> AppMeta {
        meta(Runtime::Systemd {
            unit: "asc-app-demo.service".into(),
        })
    }

    fn process() -> AppMeta {
        meta(Runtime::Process {
            command: "x".into(),
            args: vec![],
        })
    }

    fn compose() -> AppMeta {
        meta(Runtime::Compose {
            project: "asc-demo".into(),
            files: vec!["docker-compose.yml".into()],
            working_dir: "repository".into(),
        })
    }

    #[test]
    fn subprocess_log_commands_per_runtime() {
        let dir = Path::new("/asc/apps/demo");
        let docker = DockerConfig::default();
        let systemd = build_logs_command(&systemd(), dir, 50, &docker, &RunStart::Unknown).unwrap();
        assert_eq!(systemd.as_std().get_program(), "journalctl");
        assert_eq!(
            args(&systemd),
            [
                "-u",
                "asc-app-demo.service",
                "-f",
                "-n",
                "50",
                "-o",
                "short-iso",
                "--no-pager"
            ]
        );

        let process = build_logs_command(&process(), dir, 50, &docker, &RunStart::Unknown).unwrap();
        assert_eq!(process.as_std().get_program(), "tail");
        assert_eq!(args(&process), ["-n", "50", "-F", "/asc/apps/demo/app.log"]);
    }

    #[test]
    fn systemd_follows_the_current_invocation_only() {
        let run = RunStart::Invocation("0123abcd".into());
        let cmd = build_logs_command(
            &systemd(),
            Path::new("/asc/apps/demo"),
            50,
            &DockerConfig::default(),
            &run,
        )
        .unwrap();
        assert_eq!(
            args(&cmd),
            [
                "_SYSTEMD_INVOCATION_ID=0123abcd",
                "-f",
                "-n",
                "50",
                "-o",
                "short-iso",
                "--no-pager"
            ]
        );
    }

    #[test]
    fn process_follows_from_the_run_offset() {
        let cmd = build_logs_command(
            &process(),
            Path::new("/asc/apps/demo"),
            50,
            &DockerConfig::default(),
            &RunStart::Offset(1234),
        )
        .unwrap();
        // tail's +N is 1-based.
        assert_eq!(args(&cmd), ["-c", "+1235", "-F", "/asc/apps/demo/app.log"]);
    }

    #[test]
    fn docker_has_no_subprocess_source() {
        let dir = Path::new("/asc/apps/demo");
        assert!(
            build_logs_command(
                &meta(Runtime::Docker {
                    container: "asc-demo".into(),
                    image_source: None,
                }),
                dir,
                50,
                &DockerConfig::default(),
                &RunStart::Unknown,
            )
            .is_err()
        );
    }

    #[test]
    fn compose_follows_via_the_plugin_from_the_projects_own_directory() {
        let dir = Path::new("/asc/apps/demo");
        let docker = DockerConfig::default();
        let cmd = build_logs_command(&compose(), dir, 50, &docker, &RunStart::Unknown).unwrap();
        let std_cmd = cmd.as_std();
        assert_eq!(std_cmd.get_program(), "docker");
        assert_eq!(
            std_cmd.get_current_dir(),
            Some(Path::new("/asc/apps/demo/repository"))
        );
        assert_eq!(
            args(&cmd),
            [
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

    #[test]
    fn compose_cuts_at_the_run_start_with_nanoseconds() {
        let at =
            time::OffsetDateTime::from_unix_timestamp_nanos(1_700_000_000_000_000_042).unwrap();
        let cmd = build_logs_command(
            &compose(),
            Path::new("/asc/apps/demo"),
            50,
            &DockerConfig::default(),
            &RunStart::Since(at),
        )
        .unwrap();
        let args = args(&cmd);
        assert_eq!(args[args.len() - 2..], ["--since", "1700000000.000000042"]);
    }

    fn start_of(log: &str, run_offset: u64, tail: usize) -> u64 {
        backlog_start(&mut std::io::Cursor::new(log.as_bytes()), run_offset, tail).unwrap()
    }

    #[test]
    fn backlog_starts_at_the_run_not_before_it() {
        let previous = "old 1\nold 2\n";
        let log = format!("{previous}new 1\nnew 2\n");
        let run = previous.len() as u64;
        // Fewer lines in the run than the tail: the whole run, nothing older.
        assert_eq!(start_of(&log, run, 50), run);
        // More: only the last `tail` lines of it.
        assert_eq!(
            start_of(&log, run, 1),
            (previous.len() + "new 1\n".len()) as u64
        );
        // An unterminated last line still counts as a line.
        assert_eq!(start_of("a\nb\npartial", 0, 1), 4);
        // Nothing wanted: follow from the end.
        assert_eq!(start_of(&log, run, 0), log.len() as u64);
    }

    #[test]
    fn backlog_after_truncation_takes_the_whole_file() {
        assert_eq!(start_of("fresh\n", 4096, 50), 0);
    }

    #[test]
    fn backlog_window_skips_a_leading_fragment() {
        let line = "x".repeat(99) + "\n";
        let log = line.repeat((PROCESS_BACKLOG_WINDOW_BYTES as usize / 100) + 10);
        let start = start_of(&log, 0, usize::MAX);
        // Lands on a line boundary inside the window, never mid-line.
        assert!(start >= log.len() as u64 - PROCESS_BACKLOG_WINDOW_BYTES);
        assert_eq!(start % 100, 0);
    }
}
