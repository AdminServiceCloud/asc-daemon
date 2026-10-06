//! The fail2ban model: global defaults and per-jail overrides, the
//! catalog of jails the daemon knows how to run, and the validation that keeps
//! anything an operator types out of the generated config unless it is a time,
//! a number, a port list, a path or an address.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::daemon::firewall::model::parse_net;

pub const MAX_IGNORE: usize = 200;
pub const MAX_RETRY: u32 = 1000;

/// A jail the daemon can configure.
pub struct KnownJail {
    pub name: &'static str,
    /// Needs the web server module: its logs are where the filter reads.
    pub needs_web: bool,
    /// The default `logpath`, when the stock config has none that suits (the
    /// web server's logs live under /var/log/asc/webserver).
    pub logpath: Option<&'static str>,
    /// The default `port`, `None` to leave the stock one.
    pub port: Option<&'static str>,
    /// Use `nftables-allports` instead of `nftables-multiport`.
    pub all_ports: bool,
    /// On by default after a fresh install.
    pub default_enabled: bool,
}

pub const KNOWN_JAILS: &[KnownJail] = &[
    KnownJail {
        name: "sshd",
        needs_web: false,
        logpath: None,
        port: None,
        all_ports: false,
        default_enabled: true,
    },
    KnownJail {
        name: "recidive",
        needs_web: false,
        logpath: None,
        port: None,
        all_ports: true,
        default_enabled: false,
    },
    KnownJail {
        name: "nginx-http-auth",
        needs_web: true,
        logpath: Some("/var/log/asc/webserver/*.error.log"),
        port: Some("http,https"),
        all_ports: false,
        default_enabled: false,
    },
    KnownJail {
        name: "nginx-botsearch",
        needs_web: true,
        logpath: Some("/var/log/asc/webserver/*.access.log"),
        port: Some("http,https"),
        all_ports: false,
        default_enabled: false,
    },
    KnownJail {
        name: "nginx-limit-req",
        needs_web: true,
        logpath: Some("/var/log/asc/webserver/*.error.log"),
        port: Some("http,https"),
        all_ports: false,
        default_enabled: false,
    },
];

pub fn known(name: &str) -> Option<&'static KnownJail> {
    KNOWN_JAILS.iter().find(|j| j.name == name)
}

/// `1h`, `10m`, `90`, `1w`, or `-1` (permanent, ban times only).
pub fn validate_time(value: &str, what: &str, allow_permanent: bool) -> Result<()> {
    let value = value.trim();
    if allow_permanent && value == "-1" {
        return Ok(());
    }
    let digits = value.trim_end_matches(['s', 'm', 'h', 'd', 'w']);
    let unit = &value[digits.len()..];
    if digits.is_empty() || digits.len() > 9 || !digits.chars().all(|c| c.is_ascii_digit()) {
        bail!("{what} must look like 90, 10m, 1h, 1d or 1w, not '{value}'");
    }
    if unit.len() > 1 || digits.starts_with('0') && digits.len() > 1 {
        bail!("{what} must look like 90, 10m, 1h, 1d or 1w, not '{value}'");
    }
    if digits.parse::<u64>().unwrap_or(0) == 0 {
        bail!("{what} must be more than zero");
    }
    Ok(())
}

/// Global defaults written to `[DEFAULT]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub bantime: String,
    pub findtime: String,
    pub maxretry: u32,
    /// Repeat offenders are banned for longer each time.
    pub bantime_increment: bool,
    /// Addresses and networks that are never banned.
    pub ignoreip: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            bantime: "1h".into(),
            findtime: "10m".into(),
            maxretry: 5,
            bantime_increment: true,
            ignoreip: Vec::new(),
        }
    }
}

impl Settings {
    pub fn normalize(&mut self) -> Result<()> {
        self.bantime = self.bantime.trim().to_string();
        self.findtime = self.findtime.trim().to_string();
        validate_time(&self.bantime, "bantime", true)?;
        validate_time(&self.findtime, "findtime", false)?;
        if self.maxretry == 0 || self.maxretry > MAX_RETRY {
            bail!("maxretry must be between 1 and {MAX_RETRY}");
        }
        if self.ignoreip.len() > MAX_IGNORE {
            bail!("at most {MAX_IGNORE} ignored addresses are allowed");
        }
        let mut ignore = Vec::with_capacity(self.ignoreip.len());
        for entry in &self.ignoreip {
            ignore.push(parse_net(entry)?.to_string());
        }
        ignore.sort();
        ignore.dedup();
        self.ignoreip = ignore;
        Ok(())
    }
}

/// One jail's overrides; `None` keeps the default (or the stock value).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct JailConfig {
    pub name: String,
    pub enabled: bool,
    pub maxretry: Option<u32>,
    pub bantime: Option<String>,
    pub findtime: Option<String>,
    pub port: Option<String>,
    pub logpath: Option<String>,
}

impl JailConfig {
    pub fn normalize(&mut self) -> Result<()> {
        if known(&self.name).is_none() {
            bail!(
                "unknown jail '{}' (known: {})",
                self.name,
                KNOWN_JAILS
                    .iter()
                    .map(|j| j.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if let Some(n) = self.maxretry
            && (n == 0 || n > MAX_RETRY)
        {
            bail!("maxretry must be between 1 and {MAX_RETRY}");
        }
        self.bantime = clean_optional(self.bantime.take());
        self.findtime = clean_optional(self.findtime.take());
        self.port = clean_optional(self.port.take());
        self.logpath = clean_optional(self.logpath.take());
        if let Some(v) = &self.bantime {
            validate_time(v, "bantime", true)?;
        }
        if let Some(v) = &self.findtime {
            validate_time(v, "findtime", false)?;
        }
        if let Some(v) = &self.port {
            validate_port_list(v)?;
        }
        if let Some(v) = &self.logpath {
            validate_logpath(v)?;
        }
        Ok(())
    }
}

fn clean_optional(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// `22`, `22,2222`, `http,https`, `8000:8100` — port numbers, ranges and
/// service names, nothing else.
pub fn validate_port_list(value: &str) -> Result<()> {
    let ok = !value.is_empty()
        && value.len() <= 100
        && value.split(',').all(|item| {
            !item.is_empty()
                && item
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == ':' || c == '-')
        });
    if !ok {
        bail!(
            "port must be a comma-separated list of ports, ranges or service names, not '{value}'"
        );
    }
    Ok(())
}

/// An absolute path or glob without whitespace or characters that would
/// start a new config line.
pub fn validate_logpath(value: &str) -> Result<()> {
    let ok = value.starts_with('/')
        && value.len() <= 240
        && !value.contains("..")
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-*?[]".contains(c));
    if !ok {
        bail!("logpath must be an absolute path (globs allowed) without spaces, not '{value}'");
    }
    Ok(())
}

/// The whole model, persisted as one document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Model {
    pub settings: Settings,
    pub jails: Vec<JailConfig>,
}

impl Model {
    /// The configuration of a jail, or its catalog defaults before anyone
    /// touched it.
    pub fn jail(&self, name: &str) -> JailConfig {
        self.jails
            .iter()
            .find(|j| j.name == name)
            .cloned()
            .unwrap_or_else(|| JailConfig {
                name: name.to_string(),
                enabled: known(name).is_some_and(|k| k.default_enabled),
                ..JailConfig::default()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_are_validated() {
        for ok in ["90", "10m", "1h", "1d", "1w", "30s"] {
            assert!(validate_time(ok, "t", false).is_ok(), "{ok}");
        }
        assert!(validate_time("-1", "t", true).is_ok());
        assert!(validate_time("-1", "t", false).is_err());
        for bad in [
            "", "m", "0", "0m", "1x", "1hh", "1.5h", "10 m", "1h;rm", "007",
        ] {
            assert!(validate_time(bad, "t", true).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn settings_are_normalised() {
        let mut s = Settings {
            ignoreip: vec![
                " 10.0.0.0/8 ".into(),
                "2001:DB8::1".into(),
                "10.0.0.0/8".into(),
            ],
            ..Settings::default()
        };
        s.normalize().unwrap();
        assert_eq!(s.ignoreip, ["10.0.0.0/8", "2001:db8::1"]);

        let mut bad = Settings {
            maxretry: 0,
            ..Settings::default()
        };
        assert!(bad.normalize().is_err());
        let mut bad = Settings {
            ignoreip: vec!["example.com".into()],
            ..Settings::default()
        };
        assert!(bad.normalize().is_err(), "only addresses are ignored");
    }

    #[test]
    fn jails_must_be_known() {
        let mut jail = JailConfig {
            name: "nonsense".into(),
            ..JailConfig::default()
        };
        assert!(jail.normalize().is_err());
        jail.name = "sshd".into();
        assert!(jail.normalize().is_ok());
    }

    #[test]
    fn jail_fields_cannot_inject_config_lines() {
        for (field, value) in [
            ("port", "22\nenabled = false"),
            ("port", "22 80"),
            ("logpath", "/var/log/x\nbanaction = evil"),
            ("logpath", "relative/path"),
            ("logpath", "/var/log/../../etc/shadow"),
            ("logpath", "/var/log/a b"),
            ("bantime", "1h\n[x]"),
        ] {
            let mut jail = JailConfig {
                name: "sshd".into(),
                ..JailConfig::default()
            };
            match field {
                "port" => jail.port = Some(value.into()),
                "logpath" => jail.logpath = Some(value.into()),
                _ => jail.bantime = Some(value.into()),
            }
            assert!(jail.normalize().is_err(), "{field}={value:?}");
        }
        let mut ok = JailConfig {
            name: "nginx-http-auth".into(),
            port: Some("http,https".into()),
            logpath: Some("/var/log/asc/webserver/*.error.log".into()),
            ..JailConfig::default()
        };
        assert!(ok.normalize().is_ok());
    }

    #[test]
    fn empty_overrides_mean_defaults() {
        let mut jail = JailConfig {
            name: "sshd".into(),
            bantime: Some("  ".into()),
            port: Some(String::new()),
            ..JailConfig::default()
        };
        jail.normalize().unwrap();
        assert_eq!(jail.bantime, None);
        assert_eq!(jail.port, None);
    }

    #[test]
    fn untouched_jails_use_the_catalog_defaults() {
        let model = Model::default();
        assert!(model.jail("sshd").enabled);
        assert!(!model.jail("recidive").enabled);
    }
}
