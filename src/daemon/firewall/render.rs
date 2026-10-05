//! The managed model as nft text (DMN-148). A pure function of the model,
//! the host facts and the current time: same input, same bytes, so the
//! "unapplied changes" check is a string comparison.
//!
//! Layout of `table inet asc`:
//!
//! * `input` (hook input, priority 0) — traffic to the host: blocklist, then
//!   connection tracking and loopback, allowlist, ICMP, DHCP replies, then the
//!   rules (deny before allow, so the order of the list does not matter), then
//!   the policy.
//! * `forward` (hook forward, priority -1) — only when something needs it:
//!   rules with scope `docker`, the blocklist, or `protect_docker`. Docker
//!   publishes ports through DNAT and the packets then skip `input`, so these
//!   rules match the *original* destination port of DNAT-ed connections. They
//!   run before Docker's own rules (priority 0) and `drop` is final; the
//!   daemon never edits Docker's chains.
//!
//! Every rule carries a counter and the comment `asc:<id>`, which is how the
//! counters are mapped back to rules.

use std::fmt::Write;

use super::host::{Facts, presets};
use super::model::{
    Action, Model, Net, Policy, PortRange, Protocol, Rule, SET_REF_PREFIX, SetEntry, parse_net,
    parse_ports,
};

pub const TABLE_FAMILY: &str = "inet";
pub const TABLE_NAME: &str = "asc";

/// The script that replaces `table inet asc` atomically: create it if it is
/// missing (so the delete cannot fail), delete it, define it again. One `nft
/// -f` is one transaction.
pub fn replace_script(table: &str) -> String {
    format!("table {TABLE_FAMILY} {TABLE_NAME}\ndelete table {TABLE_FAMILY} {TABLE_NAME}\n{table}")
}

/// The script that removes the managed table, whether or not it exists.
pub fn remove_script() -> String {
    format!("table {TABLE_FAMILY} {TABLE_NAME}\ndelete table {TABLE_FAMILY} {TABLE_NAME}\n")
}

/// The rules in effect: presets first, then the enabled user rules.
pub fn effective_rules(model: &Model, facts: &Facts) -> Vec<Rule> {
    let mut rules = presets(facts, &model.settings);
    rules.extend(
        model
            .rules
            .iter()
            .filter(|r| r.enabled && !r.is_preset())
            .cloned(),
    );
    rules
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Chain {
    Input,
    Forward,
}

/// An address in a rendered set: the network and, for temporary entries, how
/// many seconds it still has.
struct Element {
    net: Net,
    timeout: Option<i64>,
}

/// Renders `table inet asc { ... }`.
pub fn render(model: &Model, facts: &Facts, now: i64) -> String {
    let settings = &model.settings;
    let rules = effective_rules(model, facts);
    let host_rules: Vec<&Rule> = rules.iter().filter(|r| r.scope.host()).collect();
    let docker_rules: Vec<&Rule> = rules.iter().filter(|r| r.scope.docker()).collect();

    let allow = elements(&model.sets.allowlist, now);
    let block = elements(&model.sets.blocklist, now);
    let need_forward = !docker_rules.is_empty() || settings.protect_docker || !block.is_empty();
    let ipv6 = settings.ipv6;

    let mut out = String::new();
    let _ = writeln!(out, "table {TABLE_FAMILY} {TABLE_NAME} {{");
    for (name, list) in [("allow", &allow), ("block", &block)] {
        write_set(&mut out, name, false, list);
        if ipv6 {
            write_set(&mut out, name, true, list);
        }
    }

    // ── input ──
    let _ = writeln!(out, "\tchain input {{");
    let _ = writeln!(
        out,
        "\t\ttype filter hook input priority 0; policy {};",
        match settings.input_policy {
            Policy::Accept => "accept",
            Policy::Drop => "drop",
        }
    );
    if !ipv6 {
        let _ = writeln!(out, "\t\tmeta nfproto ipv6 accept");
    }
    write_set_rules(&mut out, "block", "drop", ipv6, "");
    let _ = writeln!(out, "\t\tct state established,related accept");
    let _ = writeln!(out, "\t\tct state invalid drop");
    let _ = writeln!(out, "\t\tiifname \"lo\" accept");
    write_set_rules(&mut out, "allow", "accept", ipv6, "");
    // Error and neighbour-discovery ICMP is not optional: without it IPv6
    // does not work and path MTU discovery breaks.
    if ipv6 {
        let _ = writeln!(
            out,
            "\t\ticmpv6 type {{ destination-unreachable, packet-too-big, time-exceeded, parameter-problem, nd-router-solicit, nd-router-advert, nd-neighbor-solicit, nd-neighbor-advert }} accept"
        );
    }
    let _ = writeln!(
        out,
        "\t\ticmp type {{ destination-unreachable, time-exceeded, parameter-problem }} accept"
    );
    if settings.allow_icmp {
        let _ = writeln!(out, "\t\ticmp type echo-request accept");
        if ipv6 {
            let _ = writeln!(out, "\t\ticmpv6 type echo-request accept");
        }
    }
    if settings.input_policy == Policy::Drop {
        // DHCP replies are not part of a tracked connection on every setup.
        let _ = writeln!(out, "\t\tudp sport 67 udp dport 68 accept");
        if ipv6 {
            let _ = writeln!(out, "\t\tudp sport 547 udp dport 546 accept");
        }
    }
    write_rules(&mut out, &host_rules, Chain::Input, ipv6);
    let _ = writeln!(out, "\t}}");

    // ── forward ──
    if need_forward {
        let _ = writeln!(out, "\tchain forward {{");
        let _ = writeln!(
            out,
            "\t\ttype filter hook forward priority -1; policy accept;"
        );
        if !ipv6 {
            let _ = writeln!(out, "\t\tmeta nfproto ipv6 accept");
        }
        write_set_rules(&mut out, "block", "drop", ipv6, "ct status dnat ");
        let _ = writeln!(out, "\t\tct state established,related accept");
        write_set_rules(&mut out, "allow", "accept", ipv6, "ct status dnat ");
        write_rules(&mut out, &docker_rules, Chain::Forward, ipv6);
        if settings.protect_docker {
            let _ = writeln!(
                out,
                "\t\tct status dnat counter drop comment \"asc:protect-docker\""
            );
        }
        let _ = writeln!(out, "\t}}");
    }

    let _ = writeln!(out, "}}");
    out
}

/// Entries still in force, deduplicated and sorted so the output is stable.
fn elements(entries: &[SetEntry], now: i64) -> Vec<Element> {
    let mut out: Vec<Element> = entries
        .iter()
        .filter(|e| !e.expired(now))
        .filter_map(|e| {
            parse_net(&e.value).ok().map(|net| Element {
                net,
                timeout: e.expires_unix.map(|at| (at - now).max(1)),
            })
        })
        .collect();
    out.sort_by_key(|e| (e.net.is_v6(), e.net.addr, e.net.prefix));
    out.dedup_by_key(|e| e.net.to_string());
    out
}

fn write_set(out: &mut String, name: &str, v6: bool, list: &[Element]) {
    let (suffix, kind) = if v6 {
        ("6", "ipv6_addr")
    } else {
        ("4", "ipv4_addr")
    };
    let _ = writeln!(out, "\tset {name}{suffix} {{");
    let _ = writeln!(out, "\t\ttype {kind}");
    let _ = writeln!(out, "\t\tflags interval,timeout");
    let _ = writeln!(out, "\t\tauto-merge");
    let items: Vec<String> = list
        .iter()
        .filter(|e| e.net.is_v6() == v6)
        .map(|e| match e.timeout {
            Some(seconds) => format!("{} timeout {seconds}s", e.net),
            None => e.net.to_string(),
        })
        .collect();
    if !items.is_empty() {
        let _ = writeln!(out, "\t\telements = {{ {} }}", items.join(", "));
    }
    let _ = writeln!(out, "\t}}");
}

/// `ip saddr @allow4 accept` and its IPv6 twin, with counters.
fn write_set_rules(out: &mut String, set: &str, verdict: &str, ipv6: bool, extra: &str) {
    let comment = if set == "allow" {
        "allowlist"
    } else {
        "blocklist"
    };
    let _ = writeln!(
        out,
        "\t\tip saddr @{set}4 {extra}counter {verdict} comment \"asc:{comment}\""
    );
    if ipv6 {
        let _ = writeln!(
            out,
            "\t\tip6 saddr @{set}6 {extra}counter {verdict} comment \"asc:{comment}\""
        );
    }
}

/// Rules of one chain, deny before allow, each expanded into the lines its
/// source families need.
fn write_rules(out: &mut String, rules: &[&Rule], chain: Chain, ipv6: bool) {
    for deny in [true, false] {
        for rule in rules
            .iter()
            .filter(|r| (r.action != Action::Accept) == deny)
        {
            for line in rule_lines(rule, chain, ipv6) {
                let _ = writeln!(out, "\t\t{line}");
            }
        }
    }
}

/// The nft rules one model rule becomes: one per source family (IPv4 and
/// IPv6 cannot share a `saddr` match), or a single one without sources.
fn rule_lines(rule: &Rule, chain: Chain, ipv6: bool) -> Vec<String> {
    let proto = match proto_match(rule, chain) {
        Some(p) => p,
        None => return Vec::new(),
    };
    let dnat = if chain == Chain::Forward {
        "ct status dnat "
    } else {
        ""
    };
    let verdict = match rule.action {
        Action::Accept => "accept",
        Action::Drop => "drop",
        Action::Reject => "reject",
    };
    let tail = format!("counter {verdict} comment \"asc:{}\"", rule.id);

    let mut clauses: Vec<String> = Vec::new();
    if rule.sources.is_empty() {
        clauses.push(String::new());
    } else {
        let mut v4 = Vec::new();
        let mut v6 = Vec::new();
        for source in &rule.sources {
            if let Some(name) = source.strip_prefix(SET_REF_PREFIX) {
                let set = if name == "blocklist" {
                    "block"
                } else {
                    "allow"
                };
                clauses.push(format!("ip saddr @{set}4 "));
                if ipv6 {
                    clauses.push(format!("ip6 saddr @{set}6 "));
                }
            } else if let Ok(net) = parse_net(source) {
                if net.is_v6() {
                    v6.push(net.to_string());
                } else {
                    v4.push(net.to_string());
                }
            }
        }
        if !v4.is_empty() {
            clauses.push(format!("ip saddr {} ", braces(&v4)));
        }
        if ipv6 && !v6.is_empty() {
            clauses.push(format!("ip6 saddr {} ", braces(&v6)));
        }
    }

    clauses
        .into_iter()
        .map(|source| format!("{dnat}{source}{proto}{tail}"))
        .collect()
}

/// The protocol and port match with its trailing space, or `None` when a
/// rule cannot apply to the chain (ICMP has no published Docker ports).
fn proto_match(rule: &Rule, chain: Chain) -> Option<String> {
    let ranges: Vec<PortRange> = parse_ports(&rule.ports).ok()?;
    let ports = if ranges.is_empty() {
        None
    } else {
        Some(braces(
            &ranges.iter().map(PortRange::to_string).collect::<Vec<_>>(),
        ))
    };
    match (rule.protocol, chain) {
        (Protocol::Any, _) => Some(String::new()),
        (Protocol::Icmp, Chain::Input) => Some("meta l4proto { icmp, ipv6-icmp } ".into()),
        (Protocol::Icmp, Chain::Forward) => None,
        (Protocol::Tcp | Protocol::Udp, Chain::Input) => {
            let name = if rule.protocol == Protocol::Tcp {
                "tcp"
            } else {
                "udp"
            };
            Some(match ports {
                Some(p) => format!("{name} dport {p} "),
                None => format!("meta l4proto {name} "),
            })
        }
        (Protocol::Tcp | Protocol::Udp, Chain::Forward) => {
            let name = if rule.protocol == Protocol::Tcp {
                "tcp"
            } else {
                "udp"
            };
            Some(match ports {
                Some(p) => format!("ct original protocol {name} ct original proto-dst {p} "),
                None => format!("ct original protocol {name} "),
            })
        }
    }
}

/// `80` for one item, `{ 80, 443 }` for several.
fn braces(items: &[String]) -> String {
    if items.len() == 1 {
        items[0].clone()
    } else {
        format!("{{ {} }}", items.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::{Policy, Scope, Settings};
    use super::*;

    fn model() -> Model {
        Model::default()
    }

    fn rule(id: &str, f: impl FnOnce(&mut Rule)) -> Rule {
        let mut r = Rule {
            id: id.into(),
            ..Rule::default()
        };
        f(&mut r);
        r.normalize().unwrap();
        r
    }

    #[test]
    fn empty_model_renders_a_safe_drop_table() {
        let text = render(&model(), &Facts::default(), 0);
        assert!(text.starts_with("table inet asc {"));
        assert!(text.contains("policy drop;"));
        assert!(text.contains("ct state established,related accept"));
        assert!(text.contains("iifname \"lo\" accept"));
        assert!(
            !text.contains("chain forward"),
            "no forward chain without docker rules"
        );
        assert!(!text.contains("elements"), "empty sets have no elements");
    }

    #[test]
    fn rendering_is_deterministic() {
        let mut m = model();
        m.rules.push(rule("a", |r| {
            r.protocol = Protocol::Tcp;
            r.ports = vec!["443".into(), "80".into()];
        }));
        let facts = Facts {
            ssh_ports: vec![22],
            api_port: Some(8420),
            web_installed: false,
        };
        assert_eq!(render(&m, &facts, 100), render(&m, &facts, 100));
    }

    #[test]
    fn presets_open_ssh_and_api() {
        let facts = Facts {
            ssh_ports: vec![22, 2222],
            api_port: Some(8420),
            web_installed: false,
        };
        let text = render(&model(), &facts, 0);
        assert!(text.contains("tcp dport { 22, 2222 } counter accept comment \"asc:preset-ssh\""));
        assert!(text.contains("tcp dport 8420 counter accept comment \"asc:preset-api\""));
    }

    #[test]
    fn deny_rules_come_before_allow_rules() {
        let mut m = model();
        m.rules.push(rule("allow-web", |r| {
            r.protocol = Protocol::Tcp;
            r.ports = vec!["80".into()];
        }));
        m.rules.push(rule("deny-ip", |r| {
            r.action = Action::Drop;
            r.sources = vec!["203.0.113.7".into()];
        }));
        let text = render(&m, &Facts::default(), 0);
        let deny = text.find("asc:deny-ip").unwrap();
        let allow = text.find("asc:allow-web").unwrap();
        assert!(deny < allow);
        assert!(text.contains("ip saddr 203.0.113.7 counter drop comment \"asc:deny-ip\""));
    }

    #[test]
    fn sources_split_by_family() {
        let mut m = model();
        m.rules.push(rule("mixed", |r| {
            r.protocol = Protocol::Tcp;
            r.ports = vec!["5432".into()];
            r.sources = vec![
                "10.0.0.0/8".into(),
                "192.168.0.0/16".into(),
                "fd00::/8".into(),
            ];
        }));
        let text = render(&m, &Facts::default(), 0);
        assert!(text.contains(
            "ip saddr { 10.0.0.0/8, 192.168.0.0/16 } tcp dport 5432 counter accept comment \"asc:mixed\""
        ));
        assert!(
            text.contains("ip6 saddr fd00::/8 tcp dport 5432 counter accept comment \"asc:mixed\"")
        );
    }

    #[test]
    fn ipv6_off_leaves_ipv6_alone() {
        let mut m = model();
        m.settings = Settings {
            ipv6: false,
            ..Settings::default()
        };
        m.rules.push(rule("v6", |r| {
            r.sources = vec!["fd00::/8".into()];
        }));
        let text = render(&m, &Facts::default(), 0);
        assert!(text.contains("meta nfproto ipv6 accept"));
        assert!(!text.contains("ip6 saddr"));
        assert!(!text.contains("set allow6"));
        assert!(!text.contains("icmpv6"));
    }

    #[test]
    fn docker_rules_match_the_original_port_of_published_traffic() {
        let mut m = model();
        m.rules.push(rule("closed", |r| {
            r.action = Action::Drop;
            r.protocol = Protocol::Tcp;
            r.ports = vec!["8080".into()];
            r.scope = Scope::Docker;
        }));
        let text = render(&m, &Facts::default(), 0);
        let forward = &text[text.find("chain forward").unwrap()..];
        assert!(forward.contains(
            "ct status dnat ct original protocol tcp ct original proto-dst 8080 counter drop comment \"asc:closed\""
        ));
        assert!(
            !text[..text.find("chain forward").unwrap()].contains("asc:closed"),
            "a docker-only rule stays out of the input chain"
        );
    }

    #[test]
    fn both_scope_lands_in_both_chains() {
        let mut m = model();
        m.rules.push(rule("web", |r| {
            r.protocol = Protocol::Tcp;
            r.ports = vec!["443".into()];
            r.scope = Scope::Both;
        }));
        let text = render(&m, &Facts::default(), 0);
        assert_eq!(text.matches("asc:web").count(), 2);
    }

    #[test]
    fn protect_docker_closes_published_ports_last() {
        let mut m = model();
        m.settings.protect_docker = true;
        let text = render(&m, &Facts::default(), 0);
        assert!(text.contains("chain forward"));
        let protect = text.find("asc:protect-docker").unwrap();
        assert!(protect > text.find("chain forward").unwrap());
    }

    #[test]
    fn set_entries_carry_remaining_time_and_skip_expired() {
        let mut m = model();
        m.sets.blocklist = vec![
            SetEntry {
                value: "1.2.3.4".into(),
                expires_unix: Some(1_000 + 3600),
                comment: String::new(),
            },
            SetEntry {
                value: "5.6.7.8".into(),
                expires_unix: Some(900),
                comment: String::new(),
            },
            SetEntry {
                value: "10.0.0.0/8".into(),
                expires_unix: None,
                comment: String::new(),
            },
        ];
        let text = render(&m, &Facts::default(), 1_000);
        assert!(text.contains("elements = { 1.2.3.4 timeout 3600s, 10.0.0.0/8 }"));
        assert!(!text.contains("5.6.7.8"));
        assert!(
            text.contains("chain forward"),
            "a blocklist guards docker ports too"
        );
    }

    #[test]
    fn set_references_expand_to_both_families() {
        let mut m = model();
        m.rules.push(rule("office", |r| {
            r.protocol = Protocol::Tcp;
            r.ports = vec!["22".into()];
            r.sources = vec!["@allowlist".into()];
        }));
        let text = render(&m, &Facts::default(), 0);
        assert!(
            text.contains("ip saddr @allow4 tcp dport 22 counter accept comment \"asc:office\"")
        );
        assert!(
            text.contains("ip6 saddr @allow6 tcp dport 22 counter accept comment \"asc:office\"")
        );
    }

    #[test]
    fn icmp_rules_stay_out_of_the_forward_chain() {
        let r = rule("ping", |r| {
            r.protocol = Protocol::Icmp;
            r.scope = Scope::Docker;
        });
        assert!(rule_lines(&r, Chain::Forward, true).is_empty());
        assert_eq!(
            rule_lines(&r, Chain::Input, true),
            ["meta l4proto { icmp, ipv6-icmp } counter accept comment \"asc:ping\""]
        );
    }

    #[test]
    fn policy_accept_skips_the_dhcp_exception() {
        let mut m = model();
        m.settings.input_policy = Policy::Accept;
        let text = render(&m, &Facts::default(), 0);
        assert!(text.contains("policy accept;"));
        assert!(!text.contains("udp sport 67"));
    }

    #[test]
    fn scripts_replace_atomically() {
        let table = render(&model(), &Facts::default(), 0);
        let script = replace_script(&table);
        assert!(script.starts_with("table inet asc\ndelete table inet asc\ntable inet asc {"));
        assert_eq!(remove_script(), "table inet asc\ndelete table inet asc\n");
    }
}
