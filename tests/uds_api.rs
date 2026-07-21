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

#[test]
fn missing_socket_means_no_daemon() {
    let ws = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.api.socket = ws.path().join("absent.sock");
    assert!(Daemon::connect(&config).unwrap().is_none());
}
