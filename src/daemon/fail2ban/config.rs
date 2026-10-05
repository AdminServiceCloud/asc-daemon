//! The one file the daemon owns in fail2ban's configuration:
//! `/etc/fail2ban/jail.d/asc.local`. The operator's `jail.local` and the other
//! `jail.d` files are never touched; `.local` files override the stock
//! `jail.conf`, so this is also where a jail gets switched on.

use std::fmt::Write;

use super::model::{KNOWN_JAILS, Model};
use crate::daemon::firewall::host::Facts;

pub const FILE_NAME: &str = "asc.local";

/// Addresses that are never banned, whatever the operator lists.
const ALWAYS_IGNORED: &str = "127.0.0.1/8 ::1";

/// Renders the file. A jail that needs the web server is left out (and so
/// stays off) while that module is not installed.
pub fn render(model: &Model, facts: &Facts) -> String {
    let s = &model.settings;
    let mut out = String::new();
    out.push_str(
        "# Managed by asc-daemon (fail2ban module). Manual edits are overwritten;\n\
         # put your own jails in jail.local or another jail.d file.\n\n",
    );
    let _ = writeln!(out, "[DEFAULT]");
    let _ = writeln!(out, "bantime = {}", s.bantime);
    let _ = writeln!(out, "findtime = {}", s.findtime);
    let _ = writeln!(out, "maxretry = {}", s.maxretry);
    let _ = writeln!(out, "bantime.increment = {}", s.bantime_increment);
    let mut ignore = ALWAYS_IGNORED.to_string();
    for entry in &s.ignoreip {
        ignore.push(' ');
        ignore.push_str(entry);
    }
    let _ = writeln!(out, "ignoreip = {ignore}");
    // Bans go into fail2ban's own nftables table, next to the daemon's.
    let _ = writeln!(out, "banaction = nftables-multiport");
    let _ = writeln!(out, "banaction_allports = nftables-allports");

    for known in KNOWN_JAILS {
        if known.needs_web && !facts.web_installed {
            continue;
        }
        let jail = model.jail(known.name);
        let _ = writeln!(out, "\n[{}]", known.name);
        let _ = writeln!(out, "enabled = {}", jail.enabled);
        if let Some(n) = jail.maxretry {
            let _ = writeln!(out, "maxretry = {n}");
        }
        if let Some(v) = &jail.bantime {
            let _ = writeln!(out, "bantime = {v}");
        }
        if let Some(v) = &jail.findtime {
            let _ = writeln!(out, "findtime = {v}");
        }
        let port = jail.port.clone().or_else(|| {
            if known.name == "sshd" {
                Some(
                    facts
                        .ssh_ports
                        .iter()
                        .map(u16::to_string)
                        .collect::<Vec<_>>()
                        .join(","),
                )
                .filter(|p| !p.is_empty())
            } else {
                known.port.map(str::to_string)
            }
        });
        if let Some(port) = port
            && !known.all_ports
        {
            let _ = writeln!(out, "port = {port}");
        }
        if let Some(path) = jail.logpath.as_deref().or(known.logpath) {
            let _ = writeln!(out, "logpath = {path}");
        }
        if known.all_ports {
            let _ = writeln!(out, "banaction = %(banaction_allports)s");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::model::{JailConfig, Settings};
    use super::*;

    fn facts(web: bool) -> Facts {
        Facts {
            ssh_ports: vec![22, 2222],
            api_port: None,
            web_installed: web,
        }
    }

    #[test]
    fn defaults_and_the_ssh_jail_are_rendered() {
        let text = render(&Model::default(), &facts(false));
        assert!(text.contains("[DEFAULT]\nbantime = 1h\nfindtime = 10m\nmaxretry = 5\n"));
        assert!(text.contains("bantime.increment = true"));
        assert!(text.contains("ignoreip = 127.0.0.1/8 ::1\n"));
        assert!(text.contains("banaction = nftables-multiport"));
        assert!(text.contains("[sshd]\nenabled = true\nport = 22,2222\n"));
        assert!(text.contains("[recidive]\nenabled = false"));
        assert!(text.contains("banaction = %(banaction_allports)s"));
    }

    #[test]
    fn web_jails_wait_for_the_web_server() {
        let without = render(&Model::default(), &facts(false));
        assert!(!without.contains("nginx-http-auth"));
        let with = render(&Model::default(), &facts(true));
        assert!(with.contains(
            "[nginx-http-auth]\nenabled = false\nport = http,https\nlogpath = /var/log/asc/webserver/*.error.log\n"
        ));
        assert!(with.contains("logpath = /var/log/asc/webserver/*.access.log"));
    }

    #[test]
    fn overrides_and_ignored_addresses_are_written() {
        let mut model = Model {
            settings: Settings {
                bantime: "-1".into(),
                maxretry: 3,
                bantime_increment: false,
                ignoreip: vec!["203.0.113.0/24".into()],
                ..Settings::default()
            },
            ..Model::default()
        };
        model.jails.push(JailConfig {
            name: "sshd".into(),
            enabled: true,
            maxretry: Some(2),
            bantime: Some("1d".into()),
            port: Some("2200".into()),
            ..JailConfig::default()
        });
        let text = render(&model, &facts(false));
        assert!(text.contains("bantime = -1\n"));
        assert!(text.contains("bantime.increment = false"));
        assert!(text.contains("ignoreip = 127.0.0.1/8 ::1 203.0.113.0/24"));
        assert!(text.contains("[sshd]\nenabled = true\nmaxretry = 2\nbantime = 1d\nport = 2200\n"));
    }

    #[test]
    fn rendering_is_deterministic() {
        let model = Model::default();
        assert_eq!(render(&model, &facts(true)), render(&model, &facts(true)));
    }
}
