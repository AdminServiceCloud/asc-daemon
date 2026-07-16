//! `asc app clone` (DMN-019): a full copy of an app instance under a new id,
//! with a fresh runtime reprovisioned from the copy.

use std::fs;

use asc_daemon::daemon::apps::meta::{AppMeta, DesiredState, Owner, Quota, Runtime};
use asc_daemon::daemon::apps::{AppStore, UserContext};
use asc_daemon::daemon::config::Config;
use asc_daemon::daemon::pkg;

/// Builds a source app directly on disk (repository/config/data), bypassing
/// install entirely — `locate_installed`'s fast path (an asc.yaml right at
/// the repository root) needs no registry, so this is enough for `clone_app`
/// to have something real to read.
fn seed_app(store: &AppStore, id: &str, quota: Option<Quota>) -> AppMeta {
    let app_dir = store.app_dir(id).unwrap();
    fs::create_dir_all(app_dir.join("repository")).unwrap();
    fs::write(
        app_dir.join("repository/asc.yaml"),
        "name: demo\nversion: 1.0.0\ntype: native\nruntime:\n  start: ./run.sh\n",
    )
    .unwrap();
    fs::create_dir_all(app_dir.join("config")).unwrap();
    fs::create_dir_all(app_dir.join("data")).unwrap();
    fs::write(app_dir.join("data/save.txt"), b"progress=42").unwrap();

    let meta = AppMeta {
        id: id.to_string(),
        name: "Demo".into(),
        custom_name: None,
        owner: Owner {
            uid: 1000,
            name: "tester".into(),
        },
        version: Some("1.0.0".into()),
        source: Some("local:file:///demo".into()),
        package: None,
        desired_state: DesiredState::Stopped,
        quota,
        runtime: Runtime::Process {
            command: "/bin/sh".into(),
            args: vec!["-c".into(), "./run.sh".into()],
        },
    };
    store.save(&meta).unwrap();
    meta
}

#[test]
fn clone_copies_data_and_reprovisions_under_a_new_id() {
    let ws = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.daemon.data_dir = ws.path().join("data");
    config.daemon.apps_dir = ws.path().join("apps");
    let store = AppStore::new(config.daemon.apps_dir.clone());
    let ctx = UserContext {
        uid: 1000,
        name: "tester".into(),
        is_root: false,
    };

    let source = seed_app(&store, "demo", None);

    // Progress reaches 100% (copied == total) by the time clone_app returns.
    let mut last = (0u64, 0u64);
    let clone_meta = pkg::clone_app(&config, &ctx, &store, &source, None, |copied, total| {
        last = (copied, total);
    })
    .unwrap();
    assert!(last.1 > 0 && last.0 == last.1, "expected 100%: {last:?}");

    // DMN-033 numbering: the source occupies 'demo', so the clone is 'demo-2'
    // and its id doubles as the display name (no --name given).
    assert_eq!(clone_meta.id, "demo-2");
    assert_eq!(clone_meta.custom_name.as_deref(), Some("demo-2"));
    // Recorded like a suffixed install instance, for `asc app upgrade`.
    assert_eq!(clone_meta.package.as_deref(), Some("demo"));
    assert_eq!(clone_meta.owner.uid, 1000);
    assert_eq!(clone_meta.version.as_deref(), Some("1.0.0"));
    assert_eq!(clone_meta.desired_state, DesiredState::Stopped);
    match &clone_meta.runtime {
        Runtime::Process { command, args } => {
            assert_eq!(command, "/bin/sh");
            assert_eq!(args, &["-c", "./run.sh"]);
        }
        other => panic!("expected a process runtime, got {other:?}"),
    }

    // Data actually made it into the clone's own directory.
    let clone_dir = store.app_dir("demo-2").unwrap();
    assert_eq!(
        fs::read_to_string(clone_dir.join("data/save.txt")).unwrap(),
        "progress=42"
    );
    assert!(clone_dir.join("repository/asc.yaml").exists());
    assert!(clone_dir.join("config").is_dir());

    // The source is untouched.
    let source_dir = store.app_dir("demo").unwrap();
    assert!(source_dir.join("data/save.txt").exists());
    assert_eq!(store.get("demo").unwrap().unwrap().id, "demo");

    // An explicit --name wins over the generated default, and a second
    // clone gets the next free suffix.
    let clone2 = pkg::clone_app(
        &config,
        &ctx,
        &store,
        &source,
        Some("demo-backup"),
        |_, _| {},
    )
    .unwrap();
    assert_eq!(clone2.id, "demo-3");
    assert_eq!(clone2.custom_name.as_deref(), Some("demo-backup"));
}

#[test]
fn clone_recomputes_quota_from_copied_settings_not_stale_meta() {
    let ws = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.daemon.data_dir = ws.path().join("data");
    config.daemon.apps_dir = ws.path().join("apps");
    let store = AppStore::new(config.daemon.apps_dir.clone());
    let ctx = UserContext {
        uid: 1000,
        name: "tester".into(),
        is_root: false,
    };

    // meta.json says one thing (stale — as if edited via settings and not
    // yet applied by a start/restart); settings.json (just copied verbatim)
    // has no $quota override and no package quota either, so the clone
    // should end up with no quota at all, not the stale meta value.
    let stale_quota = Quota {
        cpu_cores: Some(4.0),
        ram_bytes: Some(8 << 30),
        disk_bytes: None,
    };
    let source = seed_app(&store, "demo", Some(stale_quota));

    let clone_meta = pkg::clone_app(&config, &ctx, &store, &source, None, |_, _| {}).unwrap();
    assert_eq!(
        clone_meta.quota, None,
        "quota must come from settings.json, not the possibly-stale meta.json"
    );
}
