//! The fail2ban manager against a fake `fail2ban-client`.

use std::sync::Arc;

use super::client::fake::FakeClient;
use super::*;
use crate::daemon::exec::ModuleError;

struct Fixture {
    f2b: Fail2ban,
    client: Arc<FakeClient>,
    dir: tempfile::TempDir,
}

fn fixture_with(client: FakeClient) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        jail_dir: dir.path().join("jail.d"),
        state: dir.path().join("state"),
    };
    std::fs::create_dir_all(&paths.jail_dir).unwrap();
    let client = Arc::new(client);
    let f2b = Fail2ban::with(paths, Box::new(Arc::clone(&client)));
    Fixture { f2b, client, dir }
}

fn fixture() -> Fixture {
    fixture_with(FakeClient::default())
}

fn facts(web: bool) -> Facts {
    Facts {
        ssh_ports: vec![22],
        api_port: Some(8420),
        web_installed: web,
    }
}

fn kind(err: &anyhow::Error) -> &'static str {
    match err.downcast_ref::<ModuleError>() {
        Some(ModuleError::Invalid(_)) => "invalid",
        Some(ModuleError::Precondition(_)) => "precondition",
        Some(ModuleError::NotFound(_)) => "not-found",
        None => "other",
    }
}

#[test]
fn a_node_without_fail2ban_reports_it_and_refuses_changes() {
    let f = fixture_with(FakeClient {
        missing: true,
        ..FakeClient::default()
    });
    let overview = f.f2b.overview(&facts(false)).unwrap();
    assert!(!overview.installed && !overview.running);
    assert_eq!(overview.jails.len(), KNOWN_JAILS.len());

    let err = f
        .f2b
        .update_settings(Settings::default(), &facts(false))
        .unwrap_err();
    assert_eq!(kind(&err), "precondition");
    assert_eq!(
        kind(&f.f2b.ban("sshd", "1.2.3.4").unwrap_err()),
        "precondition"
    );
    assert!(!f.f2b.paths.config_file().exists());
}

#[test]
fn changing_settings_writes_asc_local_and_reloads_a_running_server() {
    let f = fixture();
    *f.client.running.lock().unwrap() = true;
    let settings = Settings {
        bantime: "2h".into(),
        maxretry: 3,
        ..Settings::default()
    };
    let overview = f.f2b.update_settings(settings, &facts(false)).unwrap();
    assert_eq!(overview.settings.bantime, "2h");

    let text = std::fs::read_to_string(f.f2b.paths.config_file()).unwrap();
    assert!(text.contains("bantime = 2h") && text.contains("maxretry = 3"));
    assert_eq!(*f.client.reloads.lock().unwrap(), 1);
    assert_eq!(*f.client.starts.lock().unwrap(), 0);
    assert_eq!(f.f2b.load_state().model.settings.maxretry, 3);
}

#[test]
fn a_stopped_server_is_started_instead_of_reloaded() {
    let f = fixture();
    f.f2b
        .update_settings(Settings::default(), &facts(false))
        .unwrap();
    assert_eq!(*f.client.starts.lock().unwrap(), 1);
    assert_eq!(*f.client.reloads.lock().unwrap(), 0);
}

#[test]
fn a_refused_config_restores_the_previous_file_and_the_previous_model() {
    let f = fixture();
    f.f2b
        .update_settings(Settings::default(), &facts(false))
        .unwrap();
    let good = std::fs::read_to_string(f.f2b.paths.config_file()).unwrap();

    *f.client.reject_config.lock().unwrap() = true;
    let err = f
        .f2b
        .update_settings(
            Settings {
                maxretry: 9,
                ..Settings::default()
            },
            &facts(false),
        )
        .unwrap_err();
    assert_eq!(kind(&err), "precondition");
    assert_eq!(
        std::fs::read_to_string(f.f2b.paths.config_file()).unwrap(),
        good
    );
    let state = f.f2b.load_state();
    assert_eq!(
        state.model.settings.maxretry, 5,
        "the refused change is not kept"
    );
    assert!(state.last_error.contains("rejected"));
}

#[test]
fn a_refused_first_config_leaves_no_file() {
    let f = fixture();
    *f.client.reject_config.lock().unwrap() = true;
    assert!(
        f.f2b
            .update_settings(Settings::default(), &facts(false))
            .is_err()
    );
    assert!(!f.f2b.paths.config_file().exists());
}

#[test]
fn invalid_input_is_an_invalid_argument() {
    let f = fixture();
    let err = f
        .f2b
        .update_settings(
            Settings {
                bantime: "forever".into(),
                ..Settings::default()
            },
            &facts(false),
        )
        .unwrap_err();
    assert_eq!(kind(&err), "invalid");
    let err = f
        .f2b
        .upsert_jail(
            JailConfig {
                name: "made-up".into(),
                ..JailConfig::default()
            },
            &facts(false),
        )
        .unwrap_err();
    assert_eq!(kind(&err), "invalid");
}

#[test]
fn web_jails_need_the_web_server() {
    let f = fixture();
    let jail = JailConfig {
        name: "nginx-http-auth".into(),
        enabled: true,
        ..JailConfig::default()
    };
    let err = f.f2b.upsert_jail(jail.clone(), &facts(false)).unwrap_err();
    assert_eq!(kind(&err), "precondition");

    let overview = f.f2b.upsert_jail(jail, &facts(true)).unwrap();
    let view = overview
        .jails
        .iter()
        .find(|j| j.config.name == "nginx-http-auth")
        .unwrap();
    assert!(view.config.enabled && view.available);
    let text = std::fs::read_to_string(f.f2b.paths.config_file()).unwrap();
    assert!(text.contains("[nginx-http-auth]\nenabled = true"));
}

#[test]
fn switching_a_jail_off_is_allowed_without_the_web_server() {
    let f = fixture();
    let jail = JailConfig {
        name: "nginx-botsearch".into(),
        enabled: false,
        ..JailConfig::default()
    };
    f.f2b.upsert_jail(jail, &facts(false)).unwrap();
}

#[test]
fn the_overview_joins_the_catalog_with_what_is_running() {
    let f = fixture();
    *f.client.running.lock().unwrap() = true;
    *f.client.jails.lock().unwrap() = vec!["sshd".into()];
    f.client.statuses.lock().unwrap().insert(
        "sshd".into(),
        JailStatus {
            currently_banned: 2,
            total_banned: 9,
            banned_ips: vec!["1.2.3.4".into(), "5.6.7.8".into()],
            ..JailStatus::default()
        },
    );
    let overview = f.f2b.overview(&facts(false)).unwrap();
    assert!(overview.installed && overview.running);
    assert_eq!(overview.version, "1.1.0");
    let sshd = &overview.jails[0];
    assert_eq!(sshd.config.name, "sshd");
    assert!(sshd.active && sshd.config.enabled);
    assert_eq!(sshd.status.as_ref().unwrap().currently_banned, 2);
    let nginx = overview
        .jails
        .iter()
        .find(|j| j.config.name == "nginx-http-auth")
        .unwrap();
    assert!(!nginx.available && !nginx.active);
}

#[test]
fn statuses_are_cached_briefly_and_dropped_on_changes() {
    let f = fixture();
    *f.client.running.lock().unwrap() = true;
    *f.client.jails.lock().unwrap() = vec!["sshd".into()];
    f.f2b.overview(&facts(false)).unwrap();
    f.f2b.overview(&facts(false)).unwrap();
    assert_eq!(
        *f.client.status_calls.lock().unwrap(),
        1,
        "the second poll is served from the cache"
    );

    f.f2b.ban("sshd", "1.2.3.4").unwrap();
    f.f2b.overview(&facts(false)).unwrap();
    assert_eq!(
        *f.client.status_calls.lock().unwrap(),
        2,
        "a ban refreshes the numbers"
    );
}

#[test]
fn bans_can_be_added_listed_and_released() {
    let f = fixture();
    *f.client.running.lock().unwrap() = true;
    *f.client.jails.lock().unwrap() = vec!["sshd".into(), "recidive".into()];
    f.f2b.ban("sshd", " 203.0.113.7 ").unwrap();
    f.f2b.ban("recidive", "2001:DB8::1").unwrap();

    let all = f.f2b.bans(None).unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].jail, "recidive", "sorted by jail");
    assert_eq!(all[1].ip, "203.0.113.7");
    assert_eq!(f.f2b.bans(Some("sshd")).unwrap().len(), 1);

    assert!(f.f2b.unban("sshd", "203.0.113.7").unwrap());
    assert!(!f.f2b.unban("sshd", "203.0.113.7").unwrap());
    assert!(
        f.f2b.unban("", "2001:db8::1").unwrap(),
        "no jail means every jail"
    );
    assert!(f.f2b.bans(None).unwrap().is_empty());
}

#[test]
fn bans_validate_their_arguments() {
    let f = fixture();
    *f.client.running.lock().unwrap() = true;
    *f.client.jails.lock().unwrap() = vec!["sshd".into()];
    for bad in ["example.com", "10.0.0.0/8", "1.2.3", "1.2.3.4; reboot"] {
        assert_eq!(
            kind(&f.f2b.ban("sshd", bad).unwrap_err()),
            "invalid",
            "{bad}"
        );
    }
    assert_eq!(kind(&f.f2b.ban("nope", "1.2.3.4").unwrap_err()), "invalid");
    assert_eq!(
        kind(&f.f2b.ban("recidive", "1.2.3.4").unwrap_err()),
        "precondition",
        "a jail that is not running cannot ban"
    );
    assert_eq!(
        kind(&f.f2b.unban("nope", "1.2.3.4").unwrap_err()),
        "invalid"
    );
    assert_eq!(kind(&f.f2b.bans(Some("nope")).unwrap_err()), "invalid");
}

#[test]
fn the_state_file_survives_a_new_manager() {
    let f = fixture();
    f.f2b
        .update_settings(
            Settings {
                ignoreip: vec!["203.0.113.0/24".into()],
                ..Settings::default()
            },
            &facts(false),
        )
        .unwrap();
    let paths = f.f2b.paths.clone();
    let again = Fail2ban::with(paths, Box::new(Arc::clone(&f.client)));
    assert_eq!(
        again.overview(&facts(false)).unwrap().settings.ignoreip,
        ["203.0.113.0/24"]
    );
    let _ = &f.dir;
}

#[test]
fn a_reload_that_drops_the_ban_actions_is_repaired_with_a_restart() {
    let f = fixture();
    *f.client.running.lock().unwrap() = true;
    *f.client.jails.lock().unwrap() = vec!["sshd".into()];
    *f.client.reload_drops_actions.lock().unwrap() = true;

    let overview = f
        .f2b
        .update_settings(Settings::default(), &facts(false))
        .unwrap();
    assert_eq!(*f.client.reloads.lock().unwrap(), 1);
    assert_eq!(
        *f.client.restarts.lock().unwrap(),
        1,
        "the broken jail forces a restart"
    );
    assert_eq!(overview.last_error, "");
    assert!(
        !*f.client.actions_lost.lock().unwrap(),
        "the jail has its actions back"
    );
}

#[test]
fn a_healthy_reload_does_not_restart() {
    let f = fixture();
    *f.client.running.lock().unwrap() = true;
    *f.client.jails.lock().unwrap() = vec!["sshd".into()];
    f.f2b
        .update_settings(Settings::default(), &facts(false))
        .unwrap();
    assert_eq!(*f.client.restarts.lock().unwrap(), 0);
}

#[test]
fn a_restart_that_does_not_help_is_reported_not_hidden() {
    let f = fixture();
    *f.client.running.lock().unwrap() = true;
    *f.client.jails.lock().unwrap() = vec!["sshd".into()];
    *f.client.reload_drops_actions.lock().unwrap() = true;
    *f.client.restart_keeps_failing.lock().unwrap() = true;

    let overview = f
        .f2b
        .update_settings(Settings::default(), &facts(false))
        .unwrap();
    assert!(
        overview.last_error.contains("without a ban action"),
        "{}",
        overview.last_error
    );
    assert!(overview.last_error.contains("sshd"));
    assert!(
        f.f2b.paths.config_file().exists(),
        "the config itself was accepted"
    );
}

#[test]
fn jails_the_operator_runs_themselves_do_not_trigger_restarts() {
    let f = fixture();
    *f.client.running.lock().unwrap() = true;
    *f.client.jails.lock().unwrap() = vec!["my-own-jail".into()];
    *f.client.reload_drops_actions.lock().unwrap() = true;
    f.f2b
        .update_settings(Settings::default(), &facts(false))
        .unwrap();
    assert_eq!(*f.client.restarts.lock().unwrap(), 0);
}
