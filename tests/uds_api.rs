//! Unix-socket API integration (DMN-042): a real listener on a temp socket,
//! the real CLI client — verifying that the peer uid from SO_PEERCRED is
//! what scopes app visibility, with no token involved.

use std::sync::Arc;
use std::time::{Duration, Instant};

use asc_daemon::daemon::api::{self, ApiState};
use asc_daemon::daemon::apps::AppStore;
use asc_daemon::daemon::apps::meta::{AppMeta, DesiredState, Owner, Runtime};
use asc_daemon::daemon::client::Daemon;
use asc_daemon::daemon::config::Config;

fn meta(id: &str, uid: u32) -> AppMeta {
    AppMeta {
        id: id.into(),
        uuid: None,
        name: id.into(),
        custom_name: None,
        owner: Owner {
            uid,
            name: format!("user{uid}"),
        },
        version: None,
        source: None,
        package: None,
        desired_state: DesiredState::Stopped,
        quota: None,
        runtime: Runtime::Process {
            command: "true".into(),
            args: vec![],
        },
    }
}

/// Serve the UDS API on a background thread until the returned guard drops.
fn spawn_uds(state: Arc<ApiState>) -> tokio::sync::oneshot::Sender<()> {
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(api::uds::serve(state, async {
            let _ = stop_rx.await;
        }))
        .unwrap();
    });
    stop_tx
}

fn wait_for_socket(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("socket {} never came up", path.display());
}

#[test]
fn peer_uid_scopes_app_visibility_without_a_token() {
    let ws = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.daemon.data_dir = ws.path().join("data");
    config.daemon.apps_dir = ws.path().join("apps");
    config.api.socket = ws.path().join("asc.sock");

    // Two apps: ours, and another user's.
    // SAFETY: geteuid() has no preconditions and cannot fail.
    let my_uid = unsafe { libc::geteuid() };
    let store = AppStore::new(config.daemon.apps_dir.clone());
    store.save(&meta("mine", my_uid)).unwrap();
    store.save(&meta("foreign", my_uid + 1)).unwrap();

    let state = ApiState::new(config.clone(), "unused-token".into());
    let _stop = spawn_uds(state);
    wait_for_socket(&config.api.socket);

    // The socket is world-connectable — reaching it grants nothing by
    // itself (authorization is the peer uid, per request).
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&config.api.socket)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o666);
    }

    let daemon = Daemon::connect(&config)
        .expect("daemon answers")
        .expect("socket file exists");
    let apps = daemon.list().unwrap();
    if my_uid == 0 {
        // Root sees everyone's apps.
        assert_eq!(apps.len(), 2, "root visibility");
    } else {
        // A regular peer sees exactly their own apps — the foreign one is
        // filtered daemon-side from the kernel-reported uid.
        assert_eq!(
            apps.len(),
            1,
            "got: {:?}",
            apps.iter().map(|a| &a.id).collect::<Vec<_>>()
        );
        assert_eq!(apps[0].id, "mine");
        assert_eq!(apps[0].owner, format!("user{my_uid}"));
    }

    // The status counts are scoped the same way.
    let (_, _, total) = daemon.status().unwrap();
    assert_eq!(total as usize, apps.len());

    // Lifecycle authorization: someone else's app does not exist for us.
    if my_uid != 0 {
        let err = daemon.logs("foreign", 10).unwrap_err();
        assert!(
            format!("{err:#}").contains("not found") || format!("{err:#}").contains("не найдено"),
            "foreign apps must be indistinguishable from missing ones, got: {err:#}"
        );
    }
}

/// DMN-043: the settings editor works for a caller who cannot touch the app
/// tree itself — the daemon serves the schema and the current values, and
/// takes the edited ones back, validating them against that same schema.
#[test]
fn settings_round_trip_over_the_socket() {
    use asc_daemon::daemon::pkg::settings::SettingValues;

    let ws = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.daemon.data_dir = ws.path().join("data");
    config.daemon.apps_dir = ws.path().join("apps");
    config.api.socket = ws.path().join("asc.sock");

    // SAFETY: geteuid() has no preconditions and cannot fail.
    let my_uid = unsafe { libc::geteuid() };
    let store = AppStore::new(config.daemon.apps_dir.clone());
    store.save(&meta("mine", my_uid)).unwrap();
    let repo = store.app_dir("mine").unwrap().join("repository");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(
        repo.join("asc.yaml"),
        "name: mine\nversion: '1'\ntype: docker\nsettings: ./asc.settings.yaml\nruntime:\n  image: i\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("asc.settings.yaml"),
        "settings:\n  - key: greeting\n    type: string\n    default: hello\n",
    )
    .unwrap();

    let state = ApiState::new(config.clone(), "unused-token".into());
    let _stop = spawn_uds(state);
    wait_for_socket(&config.api.socket);
    let daemon = Daemon::connect(&config)
        .expect("daemon answers")
        .expect("socket file exists");

    // The schema arrives with the package defaults already merged in.
    let (file, values) = daemon.settings("mine").unwrap();
    let file = file.expect("the package declares settings");
    assert_eq!(file.settings.len(), 1);
    assert_eq!(file.settings[0].key, "greeting");
    assert_eq!(values.get("greeting").unwrap(), "hello");

    // An edit lands in the app's own settings.json...
    let mut edited = SettingValues::from_map(values.as_map().clone());
    edited.set("greeting", serde_json::json!("bonjour"));
    daemon.save_settings("mine", &edited).unwrap();
    let (_, reloaded) = daemon.settings("mine").unwrap();
    assert_eq!(reloaded.get("greeting").unwrap(), "bonjour");
    let on_disk = std::fs::read_to_string(
        store
            .app_dir("mine")
            .unwrap()
            .join("config")
            .join(SettingValues::FILE),
    )
    .unwrap();
    assert!(on_disk.contains("bonjour"), "got: {on_disk}");

    // ...but only for keys the package actually defines: the API is a trust
    // boundary, not a free-form key-value store.
    let mut bogus = SettingValues::default();
    bogus.set("not_a_setting", serde_json::json!(1));
    let err = daemon.save_settings("mine", &bogus).unwrap_err();
    assert!(
        format!("{err:#}").contains("unknown setting"),
        "got: {err:#}"
    );
}

/// DMN-043: `asc app attach` goes through the daemon's console, so the
/// caller needs neither the docker group nor access to the app tree — what
/// they do need is a console token, and the daemon issues one only for an
/// app that is theirs.
#[test]
fn console_tokens_are_scoped_to_the_callers_apps() {
    let ws = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.daemon.data_dir = ws.path().join("data");
    config.daemon.apps_dir = ws.path().join("apps");
    config.api.socket = ws.path().join("asc.sock");

    // SAFETY: geteuid() has no preconditions and cannot fail.
    let my_uid = unsafe { libc::geteuid() };
    let store = AppStore::new(config.daemon.apps_dir.clone());
    store.save(&meta("mine", my_uid)).unwrap();
    store.save(&meta("foreign", my_uid + 1)).unwrap();

    let state = ApiState::new(config.clone(), "unused-token".into());
    let _stop = spawn_uds(state);
    wait_for_socket(&config.api.socket);
    let daemon = Daemon::connect(&config)
        .expect("daemon answers")
        .expect("socket file exists");

    let token = daemon.console_token("mine", "attach").unwrap();
    assert!(!token.is_empty());
    // Each call mints its own single-use token.
    assert_ne!(token, daemon.console_token("mine", "attach").unwrap());

    if my_uid != 0 {
        let err = daemon.console_token("foreign", "attach").unwrap_err();
        assert!(
            format!("{err:#}").contains("not found") || format!("{err:#}").contains("не найдено"),
            "got: {err:#}"
        );
    }
}

#[test]
fn missing_socket_means_no_daemon() {
    let ws = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.api.socket = ws.path().join("absent.sock");
    assert!(Daemon::connect(&config).unwrap().is_none());
}
