//! The firewall's data model: settings, rules and IP sets, and the
//! validation that keeps anything an operator types out of the nft text
//! unless it is a port, an address or a plain comment.

use std::net::IpAddr;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// Rule ids and comment lengths the API accepts.
pub const MAX_RULES: usize = 500;
pub const MAX_SET_ENTRIES: usize = 10_000;
pub const MAX_COMMENT: usize = 120;
pub const MAX_ID: usize = 40;

/// Presets the daemon derives from the host (see [`super::host::Facts`]).
pub const PRESET_SSH: &str = "ssh";
pub const PRESET_API: &str = "api";
pub const PRESET_WEB: &str = "web";
/// Presets whose removal can cut the operator off the node.
pub const CRITICAL_PRESETS: &[&str] = &[PRESET_SSH, PRESET_API];
pub const ALL_PRESETS: &[&str] = &[PRESET_SSH, PRESET_API, PRESET_WEB];

/// How the node's firewall is managed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// The daemon touches nothing.
    #[default]
    Disabled,
    /// The daemon owns `table inet asc`, built from the model.
    Managed,
    /// The operator's text is the whole ruleset.
    Raw,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Managed => "managed",
            Self::Raw => "raw",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Policy {
    Accept,
    #[default]
    Drop,
}

impl Policy {
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "accept" => Ok(Self::Accept),
            "drop" => Ok(Self::Drop),
            other => bail!("policy must be accept or drop, not '{other}'"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    #[default]
    Accept,
    Drop,
    Reject,
}

impl Action {
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "accept" | "allow" => Ok(Self::Accept),
            "drop" | "deny" => Ok(Self::Drop),
            "reject" => Ok(Self::Reject),
            other => bail!("action must be accept, drop or reject, not '{other}'"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Tcp,
    Udp,
    #[default]
    Any,
    Icmp,
}

impl Protocol {
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "tcp" => Ok(Self::Tcp),
            "udp" => Ok(Self::Udp),
            "any" | "" => Ok(Self::Any),
            "icmp" => Ok(Self::Icmp),
            other => bail!("protocol must be tcp, udp, icmp or any, not '{other}'"),
        }
    }

    pub fn takes_ports(self) -> bool {
        matches!(self, Self::Tcp | Self::Udp)
    }
}

/// Which traffic a rule is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Traffic to the host itself (`input` chain).
    #[default]
    Host,
    /// Ports published by Docker containers (`forward` chain, matched on the
    /// original destination port, before DNAT).
    Docker,
    Both,
}

impl Scope {
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "host" | "" => Ok(Self::Host),
            "docker" => Ok(Self::Docker),
            "both" => Ok(Self::Both),
            other => bail!("scope must be host, docker or both, not '{other}'"),
        }
    }

    pub fn host(self) -> bool {
        matches!(self, Self::Host | Self::Both)
    }

    pub fn docker(self) -> bool {
        matches!(self, Self::Docker | Self::Both)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub input_policy: Policy,
    /// Answer ping (ICMP echo). Error and neighbour-discovery ICMP is always
    /// let through — dropping it breaks IPv6 and path MTU discovery.
    pub allow_icmp: bool,
    /// Filter IPv6 with the same rules; off leaves IPv6 unfiltered.
    pub ipv6: bool,
    /// Close the ports Docker publishes unless a rule with scope docker or
    /// both (or the allowlist) opens them.
    pub protect_docker: bool,
    /// Presets the operator switched off (`ssh`, `api`, `web`).
    pub disabled_presets: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            input_policy: Policy::Drop,
            allow_icmp: true,
            ipv6: true,
            protect_docker: false,
            disabled_presets: Vec::new(),
        }
    }
}

impl Settings {
    pub fn validate(&self) -> Result<()> {
        for preset in &self.disabled_presets {
            if !ALL_PRESETS.contains(&preset.as_str()) {
                bail!("unknown preset '{preset}' (known: ssh, api, web)");
            }
        }
        Ok(())
    }

    pub fn preset_enabled(&self, name: &str) -> bool {
        !self.disabled_presets.iter().any(|p| p == name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rule {
    pub id: String,
    pub enabled: bool,
    pub action: Action,
    pub protocol: Protocol,
    /// `"80"` or `"8000-8100"`.
    pub ports: Vec<String>,
    /// Addresses (`1.2.3.4`, `10.0.0.0/8`, `2001:db8::/32`); empty is "any".
    pub sources: Vec<String>,
    pub scope: Scope,
    pub comment: String,
    /// `user`, or `preset:<name>` for the derived rules.
    pub managed_by: String,
}

impl Default for Rule {
    fn default() -> Self {
        Self {
            id: String::new(),
            enabled: true,
            action: Action::Accept,
            protocol: Protocol::Any,
            ports: Vec::new(),
            sources: Vec::new(),
            scope: Scope::Host,
            comment: String::new(),
            managed_by: "user".into(),
        }
    }
}

impl Rule {
    /// Checks and normalises the rule (trimmed, canonical ports and
    /// addresses). The id is assigned by the manager when empty.
    pub fn normalize(&mut self) -> Result<()> {
        if self.id.len() > MAX_ID
            || !self
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            bail!("rule id must be up to {MAX_ID} letters, digits, '-' or '_'");
        }
        self.comment = clean_comment(&self.comment)?;
        if !self.protocol.takes_ports() && !self.ports.is_empty() {
            bail!("ports apply to tcp and udp rules only");
        }
        let ranges = parse_ports(&self.ports)?;
        self.ports = ranges.iter().map(PortRange::to_string).collect();
        let mut sources = Vec::with_capacity(self.sources.len());
        for source in &self.sources {
            sources.push(normalize_source(source)?);
        }
        sources.sort();
        sources.dedup();
        self.sources = sources;
        if self.managed_by.is_empty() {
            self.managed_by = "user".into();
        }
        Ok(())
    }

    pub fn is_preset(&self) -> bool {
        self.managed_by.starts_with("preset:")
    }
}

/// An inclusive port range; a single port has `from == to`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortRange {
    pub from: u16,
    pub to: u16,
}

impl std::fmt::Display for PortRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.from == self.to {
            write!(f, "{}", self.from)
        } else {
            write!(f, "{}-{}", self.from, self.to)
        }
    }
}

impl PortRange {
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        let port = |s: &str| -> Result<u16> {
            match s.trim().parse::<u16>() {
                Ok(0) | Err(_) => bail!("'{text}' is not a valid port or port range"),
                Ok(p) => Ok(p),
            }
        };
        match text.split_once('-') {
            Some((a, b)) => {
                let (from, to) = (port(a)?, port(b)?);
                if from > to {
                    bail!("port range '{text}' is reversed");
                }
                Ok(Self { from, to })
            }
            None => {
                let p = port(text)?;
                Ok(Self { from: p, to: p })
            }
        }
    }
}

/// Parses a port list, accepting comma-separated items inside one entry
/// (`"80,443"`), and returns the ranges sorted and without duplicates.
pub fn parse_ports(ports: &[String]) -> Result<Vec<PortRange>> {
    let mut out = Vec::new();
    for entry in ports {
        for item in entry.split(',').filter(|i| !i.trim().is_empty()) {
            out.push(PortRange::parse(item)?);
        }
    }
    out.sort_by_key(|r| (r.from, r.to));
    out.dedup();
    Ok(out)
}

/// A reference to one of the two sets, as a source: `@allowlist`.
pub const SET_REF_PREFIX: char = '@';

/// Canonical form of a rule source: an IP, a CIDR, or a set reference.
pub fn normalize_source(source: &str) -> Result<String> {
    let source = source.trim();
    if source.is_empty() {
        bail!("an empty source address");
    }
    if let Some(name) = source.strip_prefix(SET_REF_PREFIX) {
        return match name {
            "allowlist" | "blocklist" => Ok(source.to_string()),
            other => bail!("unknown set '@{other}' (known: @allowlist, @blocklist)"),
        };
    }
    Ok(parse_net(source)?.to_string())
}

/// A parsed address or network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Net {
    pub addr: IpAddr,
    /// `None` for a single host.
    pub prefix: Option<u8>,
}

impl std::fmt::Display for Net {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.prefix {
            Some(p) => write!(f, "{}/{p}", self.addr),
            None => write!(f, "{}", self.addr),
        }
    }
}

impl Net {
    pub fn is_v6(&self) -> bool {
        self.addr.is_ipv6()
    }
}

pub fn parse_net(text: &str) -> Result<Net> {
    let text = text.trim();
    let (addr, prefix) = match text.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (text, None),
    };
    let addr: IpAddr = addr
        .parse()
        .map_err(|_| anyhow::anyhow!("'{text}' is not an IP address or CIDR"))?;
    let prefix = match prefix {
        None => None,
        Some(p) => {
            let max = if addr.is_ipv6() { 128 } else { 32 };
            let value: u8 = p
                .parse()
                .ok()
                .filter(|v| *v <= max)
                .ok_or_else(|| anyhow::anyhow!("'{text}' has an invalid prefix length"))?;
            Some(value)
        }
    };
    Ok(Net { addr, prefix })
}

/// A comment that is safe inside an nft string: no quotes, backslashes or
/// control characters; trimmed; bounded.
pub fn clean_comment(comment: &str) -> Result<String> {
    let comment = comment.trim();
    if comment.chars().count() > MAX_COMMENT {
        bail!("a comment is limited to {MAX_COMMENT} characters");
    }
    if comment
        .chars()
        .any(|c| c.is_control() || c == '"' || c == '\\' || c == '\'')
    {
        bail!("a comment may not contain quotes, backslashes or control characters");
    }
    Ok(comment.to_string())
}

/// One address in an allowlist or blocklist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SetEntry {
    pub value: String,
    /// Unix time the entry stops applying; `None` for permanent.
    pub expires_unix: Option<i64>,
    pub comment: String,
}

impl SetEntry {
    pub fn normalize(&mut self) -> Result<()> {
        self.value = parse_net(&self.value)?.to_string();
        self.comment = clean_comment(&self.comment)?;
        Ok(())
    }

    pub fn expired(&self, now: i64) -> bool {
        self.expires_unix.is_some_and(|at| at <= now)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetName {
    Allowlist,
    Blocklist,
}

impl SetName {
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "allowlist" | "allow" => Ok(Self::Allowlist),
            "blocklist" | "block" => Ok(Self::Blocklist),
            other => bail!("set must be allowlist or blocklist, not '{other}'"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct IpSets {
    pub allowlist: Vec<SetEntry>,
    pub blocklist: Vec<SetEntry>,
}

impl IpSets {
    pub fn get_mut(&mut self, name: SetName) -> &mut Vec<SetEntry> {
        match name {
            SetName::Allowlist => &mut self.allowlist,
            SetName::Blocklist => &mut self.blocklist,
        }
    }
}

/// The whole managed model, persisted as one document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Model {
    pub settings: Settings,
    pub rules: Vec<Rule>,
    pub sets: IpSets,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_are_parsed_sorted_and_deduplicated() {
        let ports = parse_ports(&["443".into(), "80, 8000-8100".into(), "80".into()]).unwrap();
        let text: Vec<String> = ports.iter().map(PortRange::to_string).collect();
        assert_eq!(text, ["80", "443", "8000-8100"]);
    }

    #[test]
    fn bad_ports_are_rejected() {
        for bad in ["0", "70000", "x", "90-80", "", "80-"] {
            let mut rule = Rule {
                protocol: Protocol::Tcp,
                ports: vec![bad.into()],
                ..Rule::default()
            };
            if bad.is_empty() {
                // An empty entry is just "no ports".
                assert!(rule.normalize().is_ok());
            } else {
                assert!(rule.normalize().is_err(), "{bad} must be rejected");
            }
        }
    }

    #[test]
    fn ports_need_tcp_or_udp() {
        let mut rule = Rule {
            protocol: Protocol::Icmp,
            ports: vec!["80".into()],
            ..Rule::default()
        };
        assert!(rule.normalize().is_err());
    }

    #[test]
    fn sources_are_canonical_and_set_refs_checked() {
        let mut rule = Rule {
            sources: vec![
                "10.0.0.0/8".into(),
                " 2001:DB8::1 ".into(),
                "@blocklist".into(),
                "10.0.0.0/8".into(),
            ],
            ..Rule::default()
        };
        rule.normalize().unwrap();
        assert_eq!(rule.sources, ["10.0.0.0/8", "2001:db8::1", "@blocklist"]);
        assert!(normalize_source("@nope").is_err());
        assert!(normalize_source("10.0.0.0/33").is_err());
        assert!(normalize_source("1.2.3.4; drop").is_err());
    }

    #[test]
    fn comments_cannot_break_out_of_the_nft_string() {
        assert!(clean_comment("ok text").is_ok());
        for bad in ["a\"b", "a\\b", "a\nb", "it's"] {
            assert!(clean_comment(bad).is_err(), "{bad:?}");
        }
        assert!(clean_comment(&"x".repeat(MAX_COMMENT + 1)).is_err());
    }

    #[test]
    fn rule_ids_are_restricted() {
        let mut rule = Rule {
            id: "bad id".into(),
            ..Rule::default()
        };
        assert!(rule.normalize().is_err());
        rule.id = "r-1_a".into();
        assert!(rule.normalize().is_ok());
    }

    #[test]
    fn unknown_presets_are_rejected() {
        let settings = Settings {
            disabled_presets: vec!["telnet".into()],
            ..Settings::default()
        };
        assert!(settings.validate().is_err());
    }
}
