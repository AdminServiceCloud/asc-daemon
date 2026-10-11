//! In-place upgrade of a docker compose app. Gated behind
//! `ASC_DAEMON_TEST_DOCKER=1` since it needs a live Docker daemon **and**
//! the `docker compose` CLI plugin — mirrors `tests/install_compose.rs`.
//! Each test uses its own app id, hence its own compose project, so they can
//! run side by side.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;

use asc_daemon::daemon::apps::compose_override::{OverrideFile, ServiceOverride};
use asc_daemon::daemon::apps::meta::Runtime;
use asc_daemon::daemon::apps::{AppManager, AppStore, RuntimeState, UserContext};
use asc_daemon::daemon::config::Config;
use asc_daemon::daemon::docker;
use asc_daemon::daemon::pkg::{self, GitRef, InstallMethod, UpgradeOutcome};

fn docker_enabled() -> bool {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: git is not available");
        return false;
    }
    if std::env::var("ASC_DAEMON_TEST_DOCKER").as_deref() != Ok("1") {
        eprintln!(
            "skipping: set ASC_DAEMON_TEST_DOCKER=1 to run (needs a live Docker daemon + compose plugin)"
        );
        return false;
    }
    true
}

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

fn head(dir: &Path) -> String {
    let out = Command::new("git")
        .args(["-c", &format!("safe.directory={}", dir.display())])
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

const COMPOSE: &str = "services:
  web:
    build: .
    command: [\"sh\", \"-c\", \"cat /version; sleep 3600\"]
    volumes:
      - ./data:/data
  db:
    image: alpine:3.20
    command: [\"sleep\", \"3600\"]
";

fn dockerfile(version: &str) -> String {
    format!("FROM alpine:3.20\nRUN echo {version} > /version\n")
}

/// A repository on branch `main` with a built `web` and a pulled `db`.
fn seed_repo(repo: &Path) {
    fs::create_dir_all(repo).unwrap();
    fs::write(repo.join("docker-compose.yml"), COMPOSE).unwrap();
    fs::write(repo.join("Dockerfile"), dockerfile("v1")).unwrap();
    fs::write(repo.join("app.conf"), "mode=default\n").unwrap();
    fs::write(repo.join(".gitignore"), "data/\n").unwrap();
    git(repo, &["init", "-q", "-b", "main"]);
    git(repo, &["add", "."]);
    git(repo, &["commit", "-q", "-m", "v1"]);
}

fn commit_dockerfile(repo: &Path, content: &str, message: &str) -> String {
    fs::write(repo.join("Dockerfile"), content).unwrap();
    fs::write(repo.join("app.conf"), format!("mode={message}\n")).unwrap();
    git(repo, &["commit", "-q", "-am", message]);
    head(repo)
}

struct Env {
    _ws: tempfile::TempDir,
    repo: std::path::PathBuf,
    config: Config,
    ctx: UserContext,
    store: AppStore,
    manager: AppManager,
}

fn env(name: &str) -> Env {
    let ws = tempfile::tempdir().unwrap();
    let repo = ws.path().join(name);
    seed_repo(&repo);
    let mut config = Config::default();
    config.daemon.data_dir = ws.path().join("data");
    config.daemon.apps_dir = ws.path().join("apps");
    let ctx = UserContext {
        uid: 0,
        name: "root".into(),
        is_root: true,
    };
    let store = AppStore::new(config.daemon.apps_dir.clone());
    let manager = AppManager::new(&config);
    Env {
        _ws: ws,
        repo,
        config,
        ctx,
        store,
        manager,
    }
}

fn install(env: &Env, git_ref: Option<GitRef<'_>>) -> String {
    let url = env.repo.display().to_string().replace('\\', "/");
    let outcome = pkg::install_from_git(
        &env.config,
        &env.ctx,
        &url,
        git_ref,
        None,
        None,
        None,
        true,
        None,
        false,
        Some(InstallMethod::DockerCompose),
        None,
    )
    .unwrap();
    match outcome {
        pkg::InstallOutcome::App(report) => report.id,
        other => panic!("expected an app install, got {other:?}"),
    }
}

/// `service → (container id, state)` of the app's compose project.
fn containers(env: &Env, id: &str) -> HashMap<String, (String, String)> {
    docker::list_project_containers(&env.config.docker, &format!("asc-{id}"))
        .unwrap()
        .into_iter()
        .filter_map(|c| Some((c.compose_service.clone()?, (c.id, c.state))))
        .collect()
}

fn version_in(env: &Env, container: &str) -> String {
    let (code, out) = docker::exec_collect(
        &env.config.docker,
        container,
        &["cat".into(), "/version".into()],
    )
    .unwrap();
    assert_eq!(code, 0, "cat /version: {out}");
    out.trim().to_string()
}

fn cleanup(env: &Env, id: &str) {
    let _ = env.manager.remove(&env.ctx, id);
    let _ = docker::remove_image(&env.config.docker, &format!("asc-{id}-web"));
}

#[test]
fn a_branch_upgrade_pulls_the_new_commit_and_recreates_in_place() {
    if !docker_enabled() {
        return;
    }
    let env = env("cupg-branch");
    let id = install(&env, Some(GitRef::Branch("main")));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        env.manager.start(&env.ctx, &id).unwrap();
        let app_dir = env.store.app_dir(&id).unwrap();
        let repo_dir = app_dir.join("repository");
        let before = containers(&env, &id);
        assert_eq!(before["web"].1, "running");
        assert_eq!(version_in(&env, &before["web"].0), "v1");
        let from = head(&repo_dir);

        // The app's data lives in a relative bind mount inside the
        // repository; an operator also edited a tracked file by hand; and
        // the daemon keeps a per-service setting in its override file.
        // (The Engine created `data/` as root; the container writes into it.)
        let (code, out) = docker::exec_collect(
            &env.config.docker,
            &before["web"].0,
            &[
                "sh".into(),
                "-c".into(),
                "echo user data > /data/keep.txt".into(),
            ],
        )
        .unwrap();
        assert_eq!(code, 0, "{out}");
        fs::write(repo_dir.join("app.conf"), "mode=hand-edited\n").unwrap();
        let mut overrides = OverrideFile::default();
        overrides.services.insert(
            "web".into(),
            ServiceOverride {
                environment: [("ASC_TEST".to_string(), "kept $1".to_string())].into(),
                ..Default::default()
            },
        );
        asc_daemon::daemon::apps::compose_override::store(
            &overrides,
            &Default::default(),
            &asc_daemon::daemon::apps::compose::override_path(&app_dir),
        )
        .unwrap();

        let to = commit_dockerfile(&env.repo, &dockerfile("v2"), "v2");
        // No stop first: a compose app is upgraded while it runs.
        let outcome = pkg::upgrade(&env.config, &env.ctx, &id, None).unwrap();
        let UpgradeOutcome::Upgraded {
            from_commit,
            to_commit,
            to: version,
            ..
        } = outcome
        else {
            panic!("expected an upgrade, got {outcome:?}");
        };
        assert_eq!(from_commit.as_deref(), Some(from.as_str()));
        assert_eq!(to_commit.as_deref(), Some(to.as_str()));
        assert_eq!(version, "main");
        assert_eq!(head(&repo_dir), to);

        let after = containers(&env, &id);
        assert_eq!(after["web"].1, "running", "the stack keeps running");
        assert_ne!(after["web"].0, before["web"].0, "web is recreated");
        assert_ne!(after["db"].0, before["db"].0, "db is recreated too");
        assert_eq!(version_in(&env, &after["web"].0), "v2");

        // Untracked data survives the checkout and reaches the new
        // container; the hand edit is replaced by the new version but kept
        // as a patch.
        let (code, out) = docker::exec_collect(
            &env.config.docker,
            &after["web"].0,
            &["cat".into(), "/data/keep.txt".into()],
        )
        .unwrap();
        assert_eq!((code, out.trim()), (0, "user data"));
        assert_eq!(
            fs::read_to_string(repo_dir.join("app.conf")).unwrap(),
            "mode=v2\n"
        );
        let patches: Vec<_> = fs::read_dir(app_dir.join(".asc/backups"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".patch"))
            .collect();
        assert_eq!(patches.len(), 1);
        assert!(
            fs::read_to_string(patches[0].path())
                .unwrap()
                .contains("hand-edited")
        );

        // The override is outside the repository and still applies.
        let (code, out) = docker::exec_collect(
            &env.config.docker,
            &after["web"].0,
            &["sh".into(), "-c".into(), "echo \"$ASC_TEST\"".into()],
        )
        .unwrap();
        assert_eq!((code, out.trim()), (0, "kept $1"));

        let meta = env.store.get(&id).unwrap().unwrap();
        assert_eq!(meta.branch.as_deref(), Some("main"));
        let Runtime::Compose { services, .. } = &meta.runtime else {
            panic!("expected compose");
        };
        assert_eq!(services, &["db".to_string(), "web".to_string()]);

        // Nothing new upstream: up to date, nothing recreated.
        let outcome = pkg::upgrade(&env.config, &env.ctx, &id, None).unwrap();
        assert!(
            matches!(outcome, UpgradeOutcome::UpToDate { .. }),
            "{outcome:?}"
        );
        assert_eq!(containers(&env, &id)["web"].0, after["web"].0);

        // A rebuild on the branch pulls the next commit too, without
        // changing what the app follows.
        let v3 = commit_dockerfile(&env.repo, &dockerfile("v3"), "v3");
        let outcome = pkg::rebuild(&env.config, &env.ctx, &id, None, true, None).unwrap();
        assert!(outcome.source_updated);
        assert_eq!(outcome.from_commit.as_deref(), Some(to.as_str()));
        assert_eq!(outcome.to_commit.as_deref(), Some(v3.as_str()));
        assert!(outcome.running);
        assert_eq!(head(&repo_dir), v3);
        let rebuilt = containers(&env, &id);
        assert_ne!(rebuilt["web"].0, after["web"].0, "web is recreated");
        assert_eq!(version_in(&env, &rebuilt["web"].0), "v3");
        let meta = env.store.get(&id).unwrap().unwrap();
        assert_eq!(meta.version.as_deref(), Some("main"));
        assert_eq!(meta.branch.as_deref(), Some("main"));
    }));
    cleanup(&env, &id);
    result.unwrap();
}

#[test]
fn a_rebuild_of_a_tag_pinned_app_keeps_its_commit_and_recreates() {
    if !docker_enabled() {
        return;
    }
    let env = env("crb-tag");
    git(&env.repo, &["tag", "v1.0.0"]);
    let id = install(&env, Some(GitRef::Tag("v1.0.0")));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        env.manager.start(&env.ctx, &id).unwrap();
        let repo_dir = env.store.app_dir(&id).unwrap().join("repository");
        let pinned = head(&repo_dir);
        let before = containers(&env, &id);
        // The branch moves on; the app is pinned to the tag and stays there.
        commit_dockerfile(&env.repo, &dockerfile("v2"), "v2");

        let outcome = pkg::rebuild(&env.config, &env.ctx, &id, None, true, None).unwrap();
        assert_eq!(outcome.from_commit.as_deref(), Some(pinned.as_str()));
        assert_eq!(outcome.to_commit.as_deref(), Some(pinned.as_str()));
        assert!(!outcome.source_updated);
        assert!(outcome.running);
        assert_eq!(outcome.services, ["db".to_string(), "web".to_string()]);
        assert_eq!(head(&repo_dir), pinned);

        let after = containers(&env, &id);
        assert_ne!(after["web"].0, before["web"].0, "web is recreated");
        assert_ne!(after["db"].0, before["db"].0, "db is recreated");
        assert_eq!(after["web"].1, "running");
        assert_eq!(version_in(&env, &after["web"].0), "v1");
        let meta = env.store.get(&id).unwrap().unwrap();
        assert_eq!(meta.version.as_deref(), Some("v1.0.0"));
        assert_eq!(meta.branch, None);
    }));
    cleanup(&env, &id);
    result.unwrap();
}

#[test]
fn rebuilding_one_service_leaves_the_others_alone() {
    if !docker_enabled() {
        return;
    }
    let env = env("crb-service");
    let id = install(&env, Some(GitRef::Branch("main")));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        env.manager.start(&env.ctx, &id).unwrap();
        let before = containers(&env, &id);

        // A service cannot pull the source on its own.
        let err = pkg::rebuild(&env.config, &env.ctx, &id, Some("web"), true, None).unwrap_err();
        assert!(format!("{err:#}").contains("--no-pull"), "got: {err:#}");
        let err = pkg::rebuild(&env.config, &env.ctx, &id, Some("nope"), false, None).unwrap_err();
        assert!(format!("{err:#}").contains("nope"), "got: {err:#}");

        let outcome = pkg::rebuild(&env.config, &env.ctx, &id, Some("web"), false, None).unwrap();
        assert_eq!(outcome.services, ["web".to_string()]);
        assert!(outcome.running);
        let after = containers(&env, &id);
        assert_ne!(after["web"].0, before["web"].0, "web is recreated");
        assert_eq!(after["web"].1, "running");
        assert_eq!(after["db"], before["db"], "db is untouched");
    }));
    cleanup(&env, &id);
    result.unwrap();
}

#[test]
fn a_broken_build_keeps_the_old_containers_and_commit() {
    if !docker_enabled() {
        return;
    }
    let env = env("cupg-broken");
    let id = install(&env, Some(GitRef::Branch("main")));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        env.manager.start(&env.ctx, &id).unwrap();
        let repo_dir = env.store.app_dir(&id).unwrap().join("repository");
        let before = containers(&env, &id);
        let from = head(&repo_dir);

        commit_dockerfile(
            &env.repo,
            "FROM alpine:3.20\nRUN echo broken && exit 3\n",
            "broken",
        );
        let err = pkg::upgrade(&env.config, &env.ctx, &id, None).unwrap_err();
        assert!(
            format!("{err:#}").contains("previous version restored"),
            "got: {err:#}"
        );

        // The old commit, the old containers, still running.
        assert_eq!(head(&repo_dir), from);
        assert_eq!(
            fs::read_to_string(repo_dir.join("app.conf")).unwrap(),
            "mode=default\n"
        );
        let after = containers(&env, &id);
        assert_eq!(after["web"], before["web"]);
        assert_eq!(after["db"], before["db"]);
        assert_eq!(after["web"].1, "running");
        assert_eq!(version_in(&env, &after["web"].0), "v1");
        assert_eq!(
            env.manager.status(&env.ctx, &id).unwrap().state,
            RuntimeState::Running
        );
    }));
    cleanup(&env, &id);
    result.unwrap();
}
