//! What the firewall needs to know about the host to keep the operator from
//! locking themselves out: where SSH and the daemon API listen, and whether a
//! web server is installed. Detected by the API layer and passed in, so the
//! renderer stays a pure function and the tests need no host.

use super::model::{Action, PRESET_API, PRESET_SSH, PRESET_WEB, Protocol, Rule, Scope, Settings};
use crate::daemon::exec::{has_command, run_captured};

/// Host facts the presets are derived from.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Facts {
    pub ssh_ports: Vec<u16>,
    /// The port the daemon API listens on (TCP), if it listens on one.
    pub api_port: Option<u16>,
    pub web_installed: bool,
}

/// The rules the daemon adds on its own, minus the ones the operator
/// switched off.
pub fn presets(facts: &Facts, settings: &Settings) -> Vec<Rule> {
    let mut out = Vec::new();
    let mut add = |name: &str, protocol: Protocol, ports: Vec<u16>, comment: &str| {
        if ports.is_empty() || !settings.preset_enabled(name) {
            return;
        }
        let mut ports: Vec<String> = ports.iter().map(u16::to_string).collect();
        ports.sort();
        ports.dedup();
        out.push(Rule {
            id: format!("preset-{name}"),
            enabled: true,
            action: Action::Accept,
            protocol,
            ports,
            sources: Vec::new(),
            scope: Scope::Host,
            comment: comment.to_string(),
            managed_by: format!("preset:{name}"),
        });
    };
    add(
        PRESET_SSH,
        Protocol::Tcp,
        facts.ssh_ports.clone(),
        "SSH (preset)",
    );
    add(
        PRESET_API,
        Protocol::Tcp,
        facts.api_port.into_iter().collect(),
        "asc daemon API (preset)",
    );
    if facts.web_installed {
        add(
            PRESET_WEB,
            Protocol::Tcp,
            vec![80, 443],
            "HTTP/HTTPS (preset)",
        );
    }
    out
}

/// The value of a `<key> <port>` line, or `None` for any other line.
fn port_value(line: &str, key: &str) -> Option<u16> {
    let mut words = line.split_whitespace();
    if !words.next()?.eq_ignore_ascii_case(key) {
        return None;
    }
    words.next()?.parse().ok()
}

/// Ports from `sshd -T` output (`port 22` lines), falling back to the
/// `Port` lines of the config, then 22.
pub fn parse_sshd_ports(sshd_t: &str, config: &str) -> Vec<u16> {
    let mut ports: Vec<u16> = sshd_t
        .lines()
        .filter_map(|line| port_value(line, "port"))
        .collect();
    if ports.is_empty() {
        ports = config
            .lines()
            .filter_map(|line| port_value(line.split('#').next().unwrap_or(""), "port"))
            .collect();
    }
    if ports.is_empty() {
        ports.push(22);
    }
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// The SSH ports of this host.
pub fn detect_ssh_ports() -> Vec<u16> {
    let effective = if has_command("sshd") || std::path::Path::new("/usr/sbin/sshd").exists() {
        let sshd = if has_command("sshd") {
            "sshd"
        } else {
            "/usr/sbin/sshd"
        };
        run_captured(sshd, &["-T"])
            .ok()
            .filter(|(ok, _)| *ok)
            .map(|(_, out)| out)
            .unwrap_or_default()
    } else {
        String::new()
    };
    let config = std::fs::read_to_string("/etc/ssh/sshd_config").unwrap_or_default();
    parse_sshd_ports(&effective, &config)
}

/// The TCP port out of an API listen address such as `0.0.0.0:8420`.
pub fn api_port(listen: &str) -> Option<u16> {
    listen.rsplit(':').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sshd_ports_prefer_the_effective_config() {
        assert_eq!(
            parse_sshd_ports("port 2222\nport 22\nlistenaddress 0.0.0.0\n", "Port 9\n"),
            [22, 2222]
        );
    }

    #[test]
    fn sshd_ports_fall_back_to_the_file_then_22() {
        assert_eq!(
            parse_sshd_ports("", "#Port 22\nPort 2200 # custom\n"),
            [2200]
        );
        assert_eq!(parse_sshd_ports("", "# nothing\n"), [22]);
    }

    #[test]
    fn api_port_comes_from_the_listen_address() {
        assert_eq!(api_port("0.0.0.0:8420"), Some(8420));
        assert_eq!(api_port("[::]:9000"), Some(9000));
        assert_eq!(api_port("garbage"), None);
    }

    #[test]
    fn presets_follow_the_facts_and_can_be_disabled() {
        let facts = Facts {
            ssh_ports: vec![22],
            api_port: Some(8420),
            web_installed: true,
        };
        let mut settings = Settings::default();
        let names: Vec<_> = presets(&facts, &settings)
            .into_iter()
            .map(|r| r.managed_by)
            .collect();
        assert_eq!(names, ["preset:ssh", "preset:api", "preset:web"]);

        settings.disabled_presets = vec!["web".into(), "api".into()];
        let names: Vec<_> = presets(&facts, &settings)
            .into_iter()
            .map(|r| r.managed_by)
            .collect();
        assert_eq!(names, ["preset:ssh"]);

        let none = Facts::default();
        assert!(presets(&none, &Settings::default()).is_empty());
    }
}
