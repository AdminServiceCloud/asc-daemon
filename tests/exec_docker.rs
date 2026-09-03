//! Real Docker integration for `docker::exec` (DMN-082).
//!
//! Gated behind `ASC_DAEMON_TEST_DOCKER=1` since it needs a live Docker
//! daemon and pulls `busybox:latest` — mirrors the platform's
//! `NODESERVICE_TEST_DATABASE_URL` convention for a test that needs a live
//! external dependency `cargo test` cannot assume is present. Everything
//! else exercising this module (`tests/docker_api.rs`) runs against a mock
//! Engine instead; this file is what actually proves the bollard call
//! sequence — create_exec → start_exec → read/write the PTY → resize_exec —
//! against a real container, since a hand-rolled mock for the HTTP Upgrade
//! this needs would be a large, fragile undertaking of its own.

use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use asc_daemon::daemon::config::DockerConfig;
use asc_daemon::daemon::docker;
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;

fn docker_test_enabled() -> bool {
    std::env::var("ASC_DAEMON_TEST_DOCKER").as_deref() == Ok("1")
}

/// A disposable `busybox` container, removed on drop regardless of how the
/// test ends. `busybox` has `/bin/sh` but not `/bin/bash` — a real container
/// where the probe in `docker::exec` actually has to fall through once.
struct TestContainer {
    name: String,
}

/// `cargo test` runs every test in this file as a thread of the same
/// process, so `std::process::id()` alone is not unique per test — two
/// tests starting a container in the same instant raced on the same name
/// and one lost with "Conflict: container name already in use".
static CONTAINER_SEQ: AtomicU32 = AtomicU32::new(0);

impl TestContainer {
    fn start() -> Self {
        let seq = CONTAINER_SEQ.fetch_add(1, Ordering::Relaxed);
        let name = format!("asc-daemon-exec-test-{}-{seq}", std::process::id());
        let status = Command::new("docker")
            .args([
                "run",
                "-d",
                "--rm",
                "--name",
                &name,
                "busybox:latest",
                "sleep",
                "3600",
            ])
            .status()
            .expect("run `docker run` for the test container");
        assert!(status.success(), "failed to start the test container");
        Self { name }
    }
}

impl Drop for TestContainer {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.name])
            .status();
    }
}

#[tokio::test]
async fn exec_runs_a_command_and_reports_its_output() {
    if !docker_test_enabled() {
        eprintln!("skipping: set ASC_DAEMON_TEST_DOCKER=1 to run (needs a live Docker daemon)");
        return;
    }
    let container = TestContainer::start();
    let cfg = DockerConfig::default();

    let mut session = docker::exec(
        &cfg,
        &container.name,
        &["echo".into(), "hello-exec".into()],
        80,
        24,
    )
    .await
    .expect("exec should start");

    let mut collected = Vec::new();
    while let Some(item) = session.output.next().await {
        collected.extend_from_slice(&item.expect("exec output").into_bytes());
    }
    let text = String::from_utf8_lossy(&collected);
    assert!(text.contains("hello-exec"), "output was: {text:?}");
}

#[tokio::test]
async fn exec_resize_does_not_error_on_a_live_session() {
    if !docker_test_enabled() {
        eprintln!("skipping: set ASC_DAEMON_TEST_DOCKER=1 to run (needs a live Docker daemon)");
        return;
    }
    let container = TestContainer::start();
    let cfg = DockerConfig::default();

    let session = docker::exec(&cfg, &container.name, &["sh".into()], 80, 24)
        .await
        .expect("exec should start");
    // exec() already resized once at 80x24 during setup; this proves a
    // second, explicit resize against the same live session also works —
    // the path ws.rs's resize frame handling takes on every TagResize.
    session
        .resizer
        .resize(120, 40)
        .await
        .expect("resize a live exec session");
}

#[tokio::test]
async fn exec_probes_a_shell_when_no_command_is_given() {
    if !docker_test_enabled() {
        eprintln!("skipping: set ASC_DAEMON_TEST_DOCKER=1 to run (needs a live Docker daemon)");
        return;
    }
    let container = TestContainer::start();
    let cfg = DockerConfig::default();

    // No command: /bin/bash is tried first and this image does not have
    // it, so the probe must fall through to /bin/sh, which it does.
    let mut session = docker::exec(&cfg, &container.name, &[], 80, 24)
        .await
        .expect("the shell probe should find /bin/sh even though /bin/bash failed first");
    session
        .input
        .write_all(b"echo probed\nexit\n")
        .await
        .expect("write to the shell's stdin");
    drop(session.input); // EOF on stdin, so the shell exits after the echo

    let mut collected = Vec::new();
    while let Some(item) = session.output.next().await {
        collected.extend_from_slice(&item.expect("exec output").into_bytes());
    }
    let text = String::from_utf8_lossy(&collected);
    assert!(text.contains("probed"), "output was: {text:?}");
}
