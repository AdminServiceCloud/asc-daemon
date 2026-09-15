//! End-to-end resource shortfall / force / CPU-quota-clamp (DMN-099): the
//! cs2-on-a-1-core-node failure this feature fixes was a raw Docker 400
//! (`range of CPUs is from 0.01 to 1.00, as there are only 1 CPUs
//! available`) with no chance to intervene. These use `requirements.cpu:
//! 999` / `quota.max_cpu: 999` — a number no real test host will ever have —
//! so the shortfall triggers deterministically regardless of where this
//! runs. `type: native` throughout: quota clamping is recorded in meta.json
//! for every runtime, so this does not need Docker (unavailable in this
//! project's WSL dev environment).

use std::fs;
use std::path::Path;
use std::process::Command;

use asc_daemon::daemon::apps::{AppStore, UserContext};
use asc_daemon::daemon::config::Config;
use asc_daemon::daemon::monitor;
use asc_daemon::daemon::pkg;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git must be installed to run this test");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A repository whose manifest declares an impossible `requirements.cpu`
/// (no runtime quota) — the plain "install wants more than this machine
/// will ever have" case, independent of anything Docker would enforce.
#[test]
fn install_without_force_fails_and_with_force_succeeds() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: git is not available");
        return;
    }
    let ws = tempfile::tempdir().unwrap();
    let repo = ws.path().join("demo");
    fs::create_dir_all(&repo).unwrap();
    fs::write(
        repo.join("asc.yaml"),
        "name: demo\nversion: 1.0.0\ntype: native\nruntime:\n  start: ./run.sh\nrequirements:\n  cpu: 999\n",
    )
    .unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let url = repo.display().to_string().replace('\\', "/");

    let mut config = Config::default();
    config.daemon.data_dir = ws.path().join("data");
    config.daemon.apps_dir = ws.path().join("apps");
    let ctx = UserContext {
        uid: 1000,
        name: "tester".into(),
        is_root: false,
    };
    let store = AppStore::new(config.daemon.apps_dir.clone());

    let err = pkg::install_from_git(
        &config, &ctx, &url, None, None, None, None, true, None, false, None,
    )
    .unwrap_err();
    let not_met = err
        .downcast_ref::<pkg::RequirementsNotMet>()
        .expect("expected the typed resource-shortfall error");
    assert_eq!(not_met.app, "demo");
    assert!(
        not_met.shortages.iter().any(|s| s.resource == "CPU"),
        "got: {:?}",
        not_met.shortages
    );
    assert!(
        !store.app_dir("demo").unwrap().exists(),
        "a declined install must not leave a half-created app directory"
    );

    // --force skips the check and installs normally.
    pkg::install_from_git(
        &config, &ctx, &url, None, None, None, None, true, None, true, None,
    )
    .unwrap();
    assert!(store.get("demo").unwrap().is_some());
}

/// The exact shape of the cs2 failure: no `requirements.cpu` at all, only a
/// runtime `quota` (asc.settings.yaml, `asc-example-apps`-style) above the
/// host's total core count. `--force` must not merely skip the warning — it
/// must also clamp the quota that reaches meta.json, or the very next
/// container-create would still hit the Engine's hard `NanoCpus` ceiling.
#[test]
fn force_clamps_a_cpu_quota_above_host_capacity() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: git is not available");
        return;
    }
    let ws = tempfile::tempdir().unwrap();
    let repo = ws.path().join("demo");
    fs::create_dir_all(&repo).unwrap();
    fs::write(
        repo.join("asc.yaml"),
        "name: demo\nversion: 1.0.0\ntype: native\nsettings: ./asc.settings.yaml\nruntime:\n  start: ./run.sh\n",
    )
    .unwrap();
    fs::write(repo.join("asc.settings.yaml"), "quota:\n  max_cpu: 999\n").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let url = repo.display().to_string().replace('\\', "/");

    let mut config = Config::default();
    config.daemon.data_dir = ws.path().join("data");
    config.daemon.apps_dir = ws.path().join("apps");
    let ctx = UserContext {
        uid: 1000,
        name: "tester".into(),
        is_root: false,
    };
    let store = AppStore::new(config.daemon.apps_dir.clone());

    // Without force: the quota alone is enough to trip the same check, with
    // no `requirements` section in sight.
    let err = pkg::install_from_git(
        &config, &ctx, &url, None, None, None, None, true, None, false, None,
    )
    .unwrap_err();
    assert!(
        err.downcast_ref::<pkg::RequirementsNotMet>().is_some(),
        "got: {err:#}"
    );

    pkg::install_from_git(
        &config, &ctx, &url, None, None, None, None, true, None, true, None,
    )
    .unwrap();
    let meta = store.get("demo").unwrap().expect("meta.json must exist");
    let host_cores = monitor::system::snapshot_blocking().unwrap().cpu.cores as f64;
    assert_eq!(
        meta.quota.expect("quota recorded").cpu_cores,
        Some(host_cores),
        "the quota that reaches meta.json must be capped to what the host actually has"
    );
}
