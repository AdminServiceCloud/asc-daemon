use std::fs;
use std::os::unix::fs::PermissionsExt;

use super::*;
use crate::daemon::apps::AppStore;
use crate::daemon::apps::meta::{Owner, Runtime};

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    db: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("apps");
    let db = dir.path().join("data").join(FILE);
    fs::create_dir_all(&root).unwrap();
    Fixture {
        _dir: dir,
        root,
        db,
    }
}

fn meta(id: &str, uid: u32, custom_name: Option<&str>) -> AppMeta {
    AppMeta {
        id: id.into(),
        uuid: Some(format!("uuid-{id}")),
        name: id.into(),
        custom_name: custom_name.map(Into::into),
        owner: Owner {
            uid,
            name: format!("user{uid}"),
        },
        version: None,
        source: None,
        branch: None,
        repo_path: None,
        package: None,
        install_method: None,
        desired_state: DesiredState::Stopped,
        quota: None,
        runtime: Runtime::Process {
            command: "true".into(),
            args: vec![],
        },
    }
}

/// Write an app straight to disk, around the index.
fn write_app_files(root: &Path, meta: &AppMeta) {
    let dir = root.join(&meta.id);
    fs::create_dir_all(&dir).unwrap();
    meta.save(&dir).unwrap();
}

fn ids(index: &AppIndex) -> Vec<String> {
    index.list().unwrap().into_iter().map(|m| m.id).collect()
}

#[test]
fn open_builds_from_existing_apps_and_keeps_the_file_private() {
    let f = fixture();
    write_app_files(&f.root, &meta("bbb", 1000, None));
    write_app_files(&f.root, &meta("aaa", 1001, Some("Alpha")));

    let index = AppIndex::open(&f.db, &f.root, true).unwrap();
    assert_eq!(ids(&index), ["aaa", "bbb"]);
    let mode = fs::metadata(&f.db).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);

    let status = index.status().unwrap();
    assert_eq!(status.apps, 2);
    assert!(status.files >= 2);
    assert!(status.last_full_scan.is_some());
    assert_eq!(status.schema_version, schema::VERSION);
}

#[test]
fn sync_mirrors_every_state_file() {
    let f = fixture();
    let index = AppIndex::open(&f.db, &f.root, false).unwrap();
    write_app_files(&f.root, &meta("web", 1000, None));
    let state = f.root.join("web/.asc");
    fs::write(state.join("settings.json"), b"{\"PORT\":8080}").unwrap();
    fs::write(state.join("compose.override.yaml"), b"services: {}\n").unwrap();
    fs::write(state.join("op.lock"), b"").unwrap();

    assert_eq!(index.sync_app("web").unwrap(), SyncOutcome::Added);
    assert_eq!(
        index.state_file("web", "settings.json").unwrap(),
        StateFile::Content(b"{\"PORT\":8080}".to_vec())
    );
    assert_eq!(
        index.state_file("web", "compose.override.yaml").unwrap(),
        StateFile::Content(b"services: {}\n".to_vec())
    );
    assert_eq!(
        index.state_file("web", "op.lock").unwrap(),
        StateFile::Absent
    );
    assert_eq!(
        index.state_file("ghost", "settings.json").unwrap(),
        StateFile::NotIndexed
    );
}

#[test]
fn unmaintained_reads_see_manual_edits_additions_and_removals() {
    let f = fixture();
    write_app_files(&f.root, &meta("keep", 1000, None));
    write_app_files(&f.root, &meta("gone", 1000, None));
    let index = AppIndex::open(&f.db, &f.root, false).unwrap();
    assert_eq!(ids(&index), ["gone", "keep"]);

    // Around the index: rename by editing meta.json, add one, remove one.
    write_app_files(&f.root, &meta("keep", 1000, Some("Renamed")));
    write_app_files(&f.root, &meta("new", 1000, None));
    fs::remove_dir_all(f.root.join("gone")).unwrap();

    let listed = index.list().unwrap();
    let names: Vec<_> = listed
        .iter()
        .map(|m| (m.id.as_str(), m.display_name()))
        .collect();
    assert_eq!(names, [("keep", "Renamed"), ("new", "new")]);
}

#[test]
fn maintained_index_trusts_itself_until_verified() {
    let f = fixture();
    let index = AppIndex::open(&f.db, &f.root, true).unwrap();
    index.set_maintained();
    write_app_files(&f.root, &meta("late", 1000, None));
    // The daemon relies on its watcher and periodic verify, not on a stat
    // per read.
    assert!(ids(&index).is_empty());
    let report = index.verify().unwrap();
    assert_eq!((report.added, report.updated, report.removed), (1, 0, 0));
    assert_eq!(ids(&index), ["late"]);
    // Nothing changed: verify only stats.
    let report = index.verify().unwrap();
    assert!(!report.changed());
    assert_eq!(report.checked, 1);
}

#[test]
fn store_writes_through_and_settings_hook_updates_the_mirror() {
    let f = fixture();
    let mut config = Config::default();
    config.daemon.apps_dir = f.root.clone();
    config.daemon.data_dir = f.db.parent().unwrap().to_path_buf();
    let index = attach(&config).expect("index attaches");
    index.set_maintained();
    let store = AppStore::new(f.root.clone());

    store.save(&meta("web", 1000, None)).unwrap();
    assert_eq!(ids(&index), ["web"]);

    let mut values = crate::daemon::pkg::settings::SettingValues::default();
    values.set("MODE", serde_json::json!("prod"));
    values.save(&f.root.join("web/.asc")).unwrap();
    let raw = match index.state_file("web", "settings.json").unwrap() {
        StateFile::Content(raw) => raw,
        other => panic!("settings.json not mirrored: {other:?}"),
    };
    assert!(String::from_utf8(raw).unwrap().contains("prod"));
    assert_eq!(
        store
            .settings("web")
            .unwrap()
            .get("MODE")
            .and_then(|v| v.as_str()),
        Some("prod")
    );

    store.remove("web").unwrap();
    assert!(ids(&index).is_empty());
    assert_eq!(
        index.state_file("web", "settings.json").unwrap(),
        StateFile::NotIndexed
    );
}

#[test]
fn broken_meta_is_skipped_and_not_retried_until_it_changes() {
    let f = fixture();
    write_app_files(&f.root, &meta("good", 1000, None));
    let bad = f.root.join("bad/.asc");
    fs::create_dir_all(&bad).unwrap();
    fs::write(bad.join("meta.json"), b"{ not json").unwrap();

    let index = AppIndex::open(&f.db, &f.root, false).unwrap();
    assert_eq!(ids(&index), ["good"]);
    let report = index.verify().unwrap();
    assert!(
        !report.changed(),
        "a known-broken app must not be re-synced"
    );

    // Fixed by hand: picked up by the next verify.
    meta("bad", 1000, None).save(&f.root.join("bad")).unwrap();
    assert_eq!(ids(&index), ["bad", "good"]);
}

#[test]
fn legacy_layout_is_migrated_and_indexed_under_logical_names() {
    let f = fixture();
    let app = f.root.join("old");
    fs::create_dir_all(app.join("config")).unwrap();
    fs::write(
        app.join("meta.json"),
        serde_json::to_string(&meta("old", 1000, None)).unwrap(),
    )
    .unwrap();
    fs::write(app.join("config/settings.json"), b"{\"A\":1}").unwrap();

    let index = AppIndex::open(&f.db, &f.root, false).unwrap();
    assert_eq!(ids(&index), ["old"]);
    assert!(app.join(".asc/meta.json").exists(), "migrated into .asc/");
    assert_eq!(
        index.state_file("old", "settings.json").unwrap(),
        StateFile::Content(b"{\"A\":1}".to_vec())
    );
}

#[test]
fn broken_database_is_quarantined_and_rebuilt() {
    let f = fixture();
    write_app_files(&f.root, &meta("web", 1000, None));
    fs::create_dir_all(f.db.parent().unwrap()).unwrap();
    fs::write(&f.db, b"this is not an sqlite database, just garbage bytes").unwrap();

    let index = AppIndex::open(&f.db, &f.root, true).unwrap();
    assert_eq!(ids(&index), ["web"]);
    let quarantined = fs::read_dir(f.db.parent().unwrap())
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("index.db.broken-")
        });
    assert!(quarantined);
}

#[test]
fn newer_schema_is_set_aside() {
    let f = fixture();
    write_app_files(&f.root, &meta("web", 1000, None));
    {
        let index = AppIndex::open(&f.db, &f.root, false).unwrap();
        let conn = index.lock();
        schema::set(&conn, "version", "99").unwrap();
    }
    let index = AppIndex::open(&f.db, &f.root, false).unwrap();
    assert_eq!(index.status().unwrap().schema_version, schema::VERSION);
    assert_eq!(ids(&index), ["web"]);
}

#[test]
fn another_apps_root_rebuilds_the_index() {
    let f = fixture();
    write_app_files(&f.root, &meta("first", 1000, None));
    drop(AppIndex::open(&f.db, &f.root, false).unwrap());

    let other = f.root.parent().unwrap().join("other-apps");
    fs::create_dir_all(&other).unwrap();
    write_app_files(&other, &meta("second", 1000, None));
    let index = AppIndex::open(&f.db, &other, false).unwrap();
    assert_eq!(ids(&index), ["second"]);
}

#[test]
fn one_database_file_serves_one_root_per_process() {
    let f = fixture();
    let mut first = Config::default();
    first.daemon.apps_dir = f.root.clone();
    first.daemon.data_dir = f.db.parent().unwrap().to_path_buf();
    assert!(attach(&first).is_some());

    let mut second = first.clone();
    second.daemon.apps_dir = f.root.parent().unwrap().join("elsewhere");
    assert!(
        attach(&second).is_none(),
        "the same file must not flip between two roots"
    );
    // The first root still has its index.
    assert!(for_root(&f.root).is_some());
    assert!(
        for_root(&f.root.join(".")).is_some(),
        "lexically normalized"
    );
}

#[test]
fn commit_is_reused_while_the_git_stamp_holds() {
    let f = fixture();
    write_app_files(&f.root, &meta("web", 1000, None));
    let git = f.root.join("web/repository/.git");
    fs::create_dir_all(&git).unwrap();
    fs::write(git.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
    let index = AppIndex::open(&f.db, &f.root, false).unwrap();
    let before = index.stored_stamps_of("web").unwrap().unwrap().git;

    index.sync_app("web").unwrap();
    assert_eq!(index.stored_stamps_of("web").unwrap().unwrap().git, before);

    fs::write(
        git.join("HEAD"),
        b"0123456789abcdef0123456789abcdef01234567\n",
    )
    .unwrap();
    index.sync_app("web").unwrap();
    assert_ne!(index.stored_stamps_of("web").unwrap().unwrap().git, before);
}

#[test]
fn observed_states_are_recorded() {
    let f = fixture();
    write_app_files(&f.root, &meta("web", 1000, None));
    let index = AppIndex::open(&f.db, &f.root, false).unwrap();
    index.record_observed(&[("web".to_string(), "running")]);
    let apps = index.apps().unwrap();
    assert_eq!(apps[0].observed.as_deref(), Some("running"));
    // A sync of the files keeps the observed state.
    index.sync_app("web").unwrap();
    assert_eq!(
        index.apps().unwrap()[0].observed.as_deref(),
        Some("running")
    );
}

/// The watcher notices a change made around the daemon — a settings file
/// edited by hand, a new app directory, a removed one — without a verify.
#[cfg(target_os = "linux")]
#[test]
fn watcher_resyncs_apps_changed_around_the_daemon() {
    let f = fixture();
    write_app_files(&f.root, &meta("web", 1000, None));
    let index = Arc::new(AppIndex::open(&f.db, &f.root, true).unwrap());
    index.set_maintained();
    watch::spawn(Arc::clone(&index)).unwrap();

    let wait_for = |what: &str, check: &dyn Fn() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if check() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("the watcher never picked up: {what}");
    };

    fs::write(f.root.join("web/.asc/settings.json"), b"{\"MODE\":\"dev\"}").unwrap();
    wait_for("an edited settings.json", &|| {
        index.state_file("web", "settings.json").unwrap()
            == StateFile::Content(b"{\"MODE\":\"dev\"}".to_vec())
    });

    write_app_files(&f.root, &meta("api", 1000, None));
    wait_for("a new app directory", &|| ids(&index) == ["api", "web"]);

    fs::remove_dir_all(f.root.join("web")).unwrap();
    wait_for("a removed app directory", &|| ids(&index) == ["api"]);
}
