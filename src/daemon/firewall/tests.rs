//! Behaviour of the firewall manager against a fake `nft`: the apply and
//! rollback state machine is the part that must not lock operators out, so
//! it is tested without a kernel.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::model::{Action, Protocol};
use super::nft::fake::FakeNft;
use super::*;
use crate::daemon::exec::ModuleError;

fn no_conflicts() -> Vec<Conflict> {
    Vec::new()
}

fn nothing_to_disable(_: &[Conflict]) -> Result<()> {
    Ok(())
}

fn facts() -> Facts {
    Facts {
        ssh_ports: vec![22],
        api_port: Some(8420),
        web_installed: false,
    }
}

struct Fixture {
    fw: Arc<Firewall>,
    nft: Arc<FakeNft>,
    dir: tempfile::TempDir,
}

fn fixture_with(hooks: Hooks, nft: FakeNft) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        etc: dir.path().join("etc"),
        state: dir.path().join("state"),
        unit_dir: None,
    };
    let nft = Arc::new(nft);
    let fw = Arc::new(Firewall::with(paths, Box::new(Arc::clone(&nft)), hooks));
    Fixture { fw, nft, dir }
}

fn fixture() -> Fixture {
    fixture_with(
        Hooks {
            detect_conflicts: no_conflicts,
            disable_conflicts: nothing_to_disable,
        },
        FakeNft::default(),
    )
}

fn precondition_error(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<ModuleError>(),
        Some(ModuleError::Precondition(_))
    )
}

#[test]
fn immediate_apply_commits_and_persists() {
    let f = fixture();
    let overview = f.fw.apply(&facts(), 0, false).unwrap();
    assert_eq!(overview.mode, Mode::Managed);
    assert!(overview.pending.is_none());
    assert!(!overview.changed);
    let applied = f.nft.applied();
    assert_eq!(applied.len(), 1);
    assert!(applied[0].starts_with("table inet asc\ndelete table inet asc\ntable inet asc {"));
    let saved = std::fs::read_to_string(f.fw.paths.ruleset()).unwrap();
    assert_eq!(saved, applied[0]);
}

#[test]
fn a_windowed_change_stays_pending_until_confirmed() {
    let f = fixture();
    let overview = f.fw.apply(&facts(), 60, false).unwrap();
    assert_eq!(overview.mode, Mode::Disabled, "nothing is committed yet");
    let pending = overview.pending.expect("a pending change");
    assert!(pending.seconds_left > 0 && pending.seconds_left <= 60);
    assert!(
        !f.fw.paths.ruleset().exists(),
        "an unconfirmed ruleset is not what boots"
    );

    let overview = f.fw.confirm(&pending.id, &facts()).unwrap();
    assert_eq!(overview.mode, Mode::Managed);
    assert!(overview.pending.is_none());
    assert!(f.fw.paths.ruleset().exists());
}

#[test]
fn an_unconfirmed_change_is_rolled_back() {
    let f = fixture();
    let pending = f.fw.apply(&facts(), 30, false).unwrap().pending.unwrap();
    assert!(
        !f.fw.expire(&pending.id, pending.deadline_unix - 1).unwrap(),
        "not due yet"
    );
    assert!(f.fw.expire(&pending.id, pending.deadline_unix).unwrap());

    let applied = f.nft.applied();
    assert_eq!(applied.last().unwrap(), &render::remove_script());
    let overview = f.fw.overview(&facts()).unwrap();
    assert!(overview.pending.is_none());
    assert_eq!(overview.mode, Mode::Disabled);
    assert!(overview.last_error.contains("not confirmed"));
    assert!(!f.fw.expire(&pending.id, pending.deadline_unix + 1).unwrap());
}

#[test]
fn rollback_puts_the_previous_table_back() {
    let f = fixture();
    *f.nft.table.lock().unwrap() = Some("table inet asc {\n\tchain input {}\n}\n".into());
    f.fw.apply(&facts(), 30, false).unwrap();
    f.fw.rollback(&facts()).unwrap();
    assert_eq!(
        f.nft.applied().last().unwrap(),
        &render::replace_script("table inet asc {\n\tchain input {}\n}\n")
    );
    assert!(f.fw.overview(&facts()).unwrap().pending.is_none());
}

#[test]
fn a_second_change_waits_for_the_first() {
    let f = fixture();
    f.fw.apply(&facts(), 30, false).unwrap();
    let err = f.fw.apply(&facts(), 30, false).unwrap_err();
    assert!(precondition_error(&err), "{err:#}");
    let err =
        f.fw.apply_raw(&facts(), "table inet x {}", true, 30)
            .unwrap_err();
    assert!(precondition_error(&err), "{err:#}");
}

#[test]
fn a_late_confirmation_rolls_back_instead() {
    let f = fixture();
    let pending = f.fw.apply(&facts(), 30, false).unwrap().pending.unwrap();
    let mut stale = f.fw.load_pending().unwrap();
    stale.deadline_unix = unix_now() - 5;
    f.fw.save_pending(&stale).unwrap();

    let err = f.fw.confirm(&pending.id, &facts()).unwrap_err();
    assert!(precondition_error(&err), "{err:#}");
    assert!(f.fw.load_pending().is_none());
    assert_eq!(f.fw.overview(&facts()).unwrap().mode, Mode::Disabled);
}

#[test]
fn confirming_the_wrong_change_is_refused() {
    let f = fixture();
    f.fw.apply(&facts(), 30, false).unwrap();
    let err = f.fw.confirm("c-other", &facts()).unwrap_err();
    assert!(precondition_error(&err));
    assert!(f.fw.load_pending().is_some());
}

#[test]
fn a_rejected_ruleset_changes_nothing() {
    let f = fixture();
    *f.nft.reject.lock().unwrap() = Some("chain input".into());
    let err = f.fw.apply(&facts(), 30, false).unwrap_err();
    assert!(precondition_error(&err), "{err:#}");
    assert!(f.nft.applied().is_empty());
    assert!(f.fw.load_pending().is_none());
    assert!(!f.fw.paths.ruleset().exists());
}

static UFW_DISABLED: AtomicBool = AtomicBool::new(false);

fn ufw_active() -> Vec<Conflict> {
    vec![Conflict {
        kind: "ufw".into(),
        detail: "active".into(),
    }]
}

fn disable_ufw(found: &[Conflict]) -> Result<()> {
    assert_eq!(found[0].kind, "ufw");
    UFW_DISABLED.store(true, Ordering::SeqCst);
    Ok(())
}

#[test]
fn a_competing_firewall_blocks_enabling_unless_forced() {
    let f = fixture_with(
        Hooks {
            detect_conflicts: ufw_active,
            disable_conflicts: disable_ufw,
        },
        FakeNft::default(),
    );
    let err = f.fw.apply(&facts(), 0, false).unwrap_err();
    assert!(precondition_error(&err));
    assert!(err.to_string().contains("ufw"));
    assert!(!UFW_DISABLED.load(Ordering::SeqCst));
    assert!(f.nft.applied().is_empty());

    f.fw.apply(&facts(), 0, true).unwrap();
    assert!(UFW_DISABLED.load(Ordering::SeqCst));
    assert_eq!(f.nft.applied().len(), 1);
}

fn disable_must_not_run(_: &[Conflict]) -> Result<()> {
    panic!("the competing firewall was switched off before the ruleset was checked");
}

#[test]
fn a_failed_check_leaves_the_competing_firewall_alone() {
    let f = fixture_with(
        Hooks {
            detect_conflicts: ufw_active,
            disable_conflicts: disable_must_not_run,
        },
        FakeNft::default(),
    );
    *f.nft.reject.lock().unwrap() = Some("chain input".into());
    assert!(f.fw.apply(&facts(), 0, true).is_err());
}

#[test]
fn missing_nft_is_a_precondition_not_a_crash() {
    let f = fixture_with(
        Hooks {
            detect_conflicts: no_conflicts,
            disable_conflicts: nothing_to_disable,
        },
        FakeNft {
            missing: true,
            ..FakeNft::default()
        },
    );
    let err = f.fw.apply(&facts(), 0, false).unwrap_err();
    assert!(precondition_error(&err));
    let overview = f.fw.overview(&facts()).unwrap();
    assert!(!overview.nft_installed);
}

#[test]
fn raw_mode_needs_an_acknowledgement() {
    let f = fixture();
    let err =
        f.fw.apply_raw(&facts(), "table inet x {}", false, 0)
            .unwrap_err();
    assert!(precondition_error(&err));
    assert!(f.nft.applied().is_empty());
}

#[test]
fn raw_mode_flushes_and_rolls_back_to_the_full_dump() {
    let f = fixture();
    *f.nft.ruleset.lock().unwrap() = "table ip nat {}\n".into();
    let overview =
        f.fw.apply_raw(&facts(), "table inet mine {}\r\n", true, 30)
            .unwrap();
    assert_eq!(f.nft.applied()[0], "flush ruleset\ntable inet mine {}\n\n");
    let pending = overview.pending.unwrap();

    f.fw.rollback(&facts()).unwrap();
    assert_eq!(
        f.nft.applied().last().unwrap(),
        "flush ruleset\ntable ip nat {}\n\n"
    );
    let _ = pending;
    assert_eq!(
        f.fw.load_state().raw,
        "table inet mine {}\n",
        "the draft survives a rollback"
    );
}

#[test]
fn raw_mode_confirmed_becomes_the_boot_ruleset() {
    let f = fixture();
    f.fw.apply_raw(&facts(), "table inet mine {}", true, 0)
        .unwrap();
    let overview = f.fw.overview(&facts()).unwrap();
    assert_eq!(overview.mode, Mode::Raw);
    assert!(!overview.changed);
    assert_eq!(
        std::fs::read_to_string(f.fw.paths.ruleset()).unwrap(),
        "flush ruleset\ntable inet mine {}\n"
    );
}

#[test]
fn leaving_raw_back_to_managed_restores_the_whole_ruleset_on_rollback() {
    let f = fixture();
    f.fw.apply_raw(&facts(), "table inet mine {}", true, 0)
        .unwrap();
    *f.nft.ruleset.lock().unwrap() = "table inet mine {}\n".into();
    f.fw.apply(&facts(), 30, false).unwrap();
    f.fw.rollback(&facts()).unwrap();
    assert_eq!(
        f.nft.applied().last().unwrap(),
        "flush ruleset\ntable inet mine {}\n\n"
    );
}

#[test]
fn recover_rolls_back_a_change_that_expired_during_a_restart() {
    let f = fixture();
    f.fw.apply(&facts(), 30, false).unwrap();
    let mut stale = f.fw.load_pending().unwrap();
    stale.deadline_unix = unix_now() - 1;
    f.fw.save_pending(&stale).unwrap();

    f.fw.recover();
    assert!(f.fw.load_pending().is_none());
    assert_eq!(f.nft.applied().last().unwrap(), &render::remove_script());
}

#[test]
fn recover_keeps_a_change_that_is_still_inside_its_window() {
    let f = fixture();
    f.fw.apply(&facts(), 60, false).unwrap();
    let applied = f.nft.applied().len();
    f.fw.recover();
    assert!(f.fw.load_pending().is_some());
    assert_eq!(f.nft.applied().len(), applied);
}

#[test]
fn recover_loads_a_confirmed_table_that_is_missing() {
    let f = fixture();
    f.fw.apply(&facts(), 0, false).unwrap();
    assert!(f.nft.table.lock().unwrap().is_none());
    f.fw.recover();
    let applied = f.nft.applied();
    assert_eq!(applied.len(), 2);
    assert_eq!(applied[0], applied[1]);

    *f.nft.table.lock().unwrap() = Some("table inet asc {}".into());
    f.fw.recover();
    assert_eq!(f.nft.applied().len(), 2, "a present table is left alone");
}

#[test]
fn disable_removes_the_table_and_the_boot_file() {
    let f = fixture();
    f.fw.apply(&facts(), 0, false).unwrap();
    let overview = f.fw.disable(&facts()).unwrap();
    assert_eq!(overview.mode, Mode::Disabled);
    assert_eq!(f.nft.applied().last().unwrap(), &render::remove_script());
    assert!(!f.fw.paths.ruleset().exists());
}

#[test]
fn disable_waits_for_a_pending_change() {
    let f = fixture();
    f.fw.apply(&facts(), 30, false).unwrap();
    assert!(precondition_error(&f.fw.disable(&facts()).unwrap_err()));
}

#[test]
fn unapplied_changes_are_noticed() {
    let f = fixture();
    f.fw.apply(&facts(), 0, false).unwrap();
    f.fw.upsert_rule(Rule {
        protocol: Protocol::Tcp,
        ports: vec!["8080".into()],
        ..Rule::default()
    })
    .unwrap();
    let overview = f.fw.overview(&facts()).unwrap();
    assert!(overview.changed);
    assert!(f.fw.render(&facts()).unwrap().changed);

    f.fw.apply(&facts(), 0, false).unwrap();
    assert!(!f.fw.overview(&facts()).unwrap().changed);
}

#[test]
fn a_changed_host_fact_counts_as_a_change() {
    let f = fixture();
    f.fw.apply(&facts(), 0, false).unwrap();
    let moved = Facts {
        ssh_ports: vec![2222],
        ..facts()
    };
    assert!(f.fw.overview(&moved).unwrap().changed);
}

#[test]
fn rules_get_ids_and_are_replaced_by_id() {
    let f = fixture();
    let rule =
        f.fw.upsert_rule(Rule {
            protocol: Protocol::Tcp,
            ports: vec!["443".into()],
            ..Rule::default()
        })
        .unwrap();
    assert!(rule.id.starts_with("r-"), "{}", rule.id);

    let mut edited = rule.clone();
    edited.action = Action::Drop;
    f.fw.upsert_rule(edited).unwrap();
    let rules = f.fw.overview(&facts()).unwrap().rules;
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].action, Action::Drop);

    assert!(f.fw.remove_rule(&rule.id).unwrap());
    assert!(!f.fw.remove_rule(&rule.id).unwrap());
}

#[test]
fn preset_rules_cannot_be_stored() {
    let f = fixture();
    let err =
        f.fw.upsert_rule(Rule {
            managed_by: "preset:ssh".into(),
            ..Rule::default()
        })
        .unwrap_err();
    assert!(matches!(
        err.downcast_ref::<ModuleError>(),
        Some(ModuleError::Invalid(_))
    ));
}

#[test]
fn invalid_rules_are_invalid_arguments() {
    let f = fixture();
    let err =
        f.fw.upsert_rule(Rule {
            protocol: Protocol::Tcp,
            ports: vec!["99999".into()],
            ..Rule::default()
        })
        .unwrap_err();
    assert!(matches!(
        err.downcast_ref::<ModuleError>(),
        Some(ModuleError::Invalid(_))
    ));
}

#[test]
fn switching_off_a_critical_preset_needs_force() {
    let f = fixture();
    let settings = Settings {
        disabled_presets: vec!["ssh".into()],
        ..Settings::default()
    };
    let err = f.fw.update_settings(settings.clone(), false).unwrap_err();
    assert!(precondition_error(&err));
    f.fw.update_settings(settings, true).unwrap();

    // The web preset is not critical.
    let settings = Settings {
        disabled_presets: vec!["ssh".into(), "web".into()],
        ..Settings::default()
    };
    f.fw.update_settings(settings, false).unwrap();
}

#[test]
fn set_entries_are_normalised_and_deduplicated() {
    let f = fixture();
    let entries =
        f.fw.set_entries(
            SetName::Blocklist,
            vec![
                SetEntry {
                    value: " 203.0.113.7 ".into(),
                    ..SetEntry::default()
                },
                SetEntry {
                    value: "203.0.113.7".into(),
                    comment: "scanner".into(),
                    ..SetEntry::default()
                },
            ],
        )
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].comment, "scanner");

    assert!(
        f.fw.set_entries(
            SetName::Blocklist,
            vec![SetEntry {
                value: "nonsense".into(),
                ..SetEntry::default()
            }]
        )
        .is_err()
    );
}

#[test]
fn single_entries_can_be_added_and_removed() {
    let f = fixture();
    f.fw.add_entry(
        SetName::Allowlist,
        SetEntry {
            value: "10.0.0.0/8".into(),
            ..SetEntry::default()
        },
    )
    .unwrap();
    let overview = f.fw.overview(&facts()).unwrap();
    assert_eq!(overview.sets.allowlist.len(), 1);
    assert!(f.fw.remove_entry(SetName::Allowlist, "10.0.0.0/8").unwrap());
    assert!(!f.fw.remove_entry(SetName::Allowlist, "10.0.0.0/8").unwrap());
}

#[test]
fn expired_entries_are_dropped_when_applying() {
    let f = fixture();
    f.fw.set_entries(
        SetName::Blocklist,
        vec![
            SetEntry {
                value: "198.51.100.1".into(),
                expires_unix: Some(unix_now() - 10),
                comment: String::new(),
            },
            SetEntry {
                value: "198.51.100.2".into(),
                expires_unix: Some(unix_now() + 3600),
                comment: String::new(),
            },
        ],
    )
    .unwrap();
    f.fw.apply(&facts(), 0, false).unwrap();
    let applied = &f.nft.applied()[0];
    assert!(!applied.contains("198.51.100.1 "));
    assert!(applied.contains("198.51.100.2 timeout"));
    assert_eq!(
        f.fw.overview(&facts()).unwrap().sets.blocklist.len(),
        1,
        "the expired entry is gone from the model too"
    );
}

#[test]
fn confirmation_windows_are_bounded() {
    let f = fixture();
    for bad in [1, 14, 601, 100_000] {
        let err = f.fw.apply(&facts(), bad, false).unwrap_err();
        assert!(
            matches!(
                err.downcast_ref::<ModuleError>(),
                Some(ModuleError::Invalid(_))
            ),
            "{bad}"
        );
    }
    f.fw.apply(&facts(), 0, false).unwrap();
}

#[test]
fn a_corrupt_state_file_is_set_aside_not_overwritten() {
    let f = fixture();
    std::fs::create_dir_all(&f.fw.paths.state).unwrap();
    std::fs::write(f.fw.paths.state_file(), "{ not json").unwrap();
    let overview = f.fw.overview(&facts()).unwrap();
    assert_eq!(overview.mode, Mode::Disabled);
    assert!(
        f.dir
            .path()
            .join("state")
            .join("state.json.corrupt")
            .exists()
    );
}

#[test]
fn table_owners_are_recognised() {
    assert_eq!(table_owner("inet", "asc"), "asc");
    assert_eq!(table_owner("inet", "f2b-table"), "fail2ban");
    assert_eq!(table_owner("ip", "nat"), "iptables");
    assert_eq!(table_owner("ip", "filter"), "iptables");
    assert_eq!(table_owner("inet", "mine"), "other");
}

#[test]
fn ids_are_unique_and_shaped() {
    let a = new_id("r");
    let b = new_id("r");
    assert_ne!(a, b);
    assert_eq!(a.len(), 10);
}

/// Needs root and a kernel with nftables: `cargo test --lib -- --ignored
/// real_nft`. Asks the real `nft -c` whether it accepts what the renderer
/// produces for a model that uses every feature.
#[test]
#[ignore = "needs root and nftables"]
fn real_nft_accepts_the_rendered_ruleset() {
    use super::nft::SystemNft;
    use model::{Scope, SetEntry};

    let mut model = Model::default();
    model.settings.protect_docker = true;
    model.settings.allow_icmp = true;
    let mk = |id: &str, f: &dyn Fn(&mut Rule)| {
        let mut r = Rule {
            id: id.into(),
            ..Rule::default()
        };
        f(&mut r);
        r.normalize().unwrap();
        r
    };
    model.rules = vec![
        mk("web", &|r| {
            r.protocol = Protocol::Tcp;
            r.ports = vec!["80".into(), "443".into(), "8000-8100".into()];
            r.scope = Scope::Both;
        }),
        mk("pg", &|r| {
            r.protocol = Protocol::Tcp;
            r.ports = vec!["5432".into()];
            r.sources = vec!["10.0.0.0/8".into(), "fd00::/8".into(), "@allowlist".into()];
        }),
        mk("dns", &|r| {
            r.protocol = Protocol::Udp;
            r.ports = vec!["53".into()];
            r.comment = "resolver".into();
        }),
        mk("ping", &|r| r.protocol = Protocol::Icmp),
        mk("nope", &|r| {
            r.action = Action::Reject;
            r.protocol = Protocol::Tcp;
            r.ports = vec!["23".into()];
        }),
        mk("closed", &|r| {
            r.action = Action::Drop;
            r.protocol = Protocol::Tcp;
            r.ports = vec!["8080".into()];
            r.scope = Scope::Docker;
            r.sources = vec!["203.0.113.0/24".into()];
        }),
    ];
    let now = unix_now();
    model.sets.blocklist = vec![
        SetEntry {
            value: "198.51.100.7".into(),
            expires_unix: Some(now + 600),
            comment: String::new(),
        },
        SetEntry {
            value: "192.0.2.0/24".into(),
            expires_unix: None,
            comment: String::new(),
        },
        SetEntry {
            value: "2001:db8::/32".into(),
            expires_unix: None,
            comment: String::new(),
        },
    ];
    model.sets.allowlist = vec![SetEntry {
        value: "172.16.0.1".into(),
        expires_unix: None,
        comment: String::new(),
    }];

    let table = render::render(&model, &facts(), now);
    let nft = SystemNft;
    nft.check(&render::replace_script(&table))
        .unwrap_or_else(|err| panic!("{err:#}\n--- ruleset ---\n{table}"));

    model.settings.ipv6 = false;
    model.settings.input_policy = model::Policy::Accept;
    let table = render::render(&model, &facts(), now);
    nft.check(&render::replace_script(&table))
        .unwrap_or_else(|err| panic!("{err:#}\n--- ruleset ---\n{table}"));
}
