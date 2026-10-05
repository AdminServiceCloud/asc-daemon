//! `asc firewall` (DMN-148, DMN-149): the node's nftables firewall through
//! the running daemon. The command needs the daemon on purpose — the
//! automatic rollback is a timer inside it, and a CLI process that exits
//! right after applying could not keep that promise.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Subcommand;
use serde_json::{Value, json};

use asc_daemon::daemon::client::Daemon;
use asc_daemon::daemon::config::Config;
use asc_daemon::daemon::i18n::{Msg, t, tf, tf2};

use super::{daemon_backend, local_minute};

#[derive(Subcommand)]
pub enum FirewallAction {
    /// Show the firewall: mode, policy, rules with their counters, pending change
    Status,
    /// Install the nftables package
    Install,
    /// Turn the managed firewall on: loads table `inet asc` with an automatic rollback
    Enable(ApplyArgs),
    /// Turn the managed firewall off and remove its table
    Disable,
    /// Open ports or an address: `asc firewall allow 443/tcp`, `allow 5432/tcp --from 10.0.0.0/8`
    Allow(RuleArgs),
    /// Block ports or an address: `asc firewall deny --from 203.0.113.7`
    Deny(RuleArgs),
    /// List the rules, the ones derived from the host included
    Rules,
    /// Remove a rule
    Remove {
        /// Rule id (see `asc firewall rules`)
        id: String,
        /// Store the change without applying it
        #[arg(long)]
        no_apply: bool,
        #[command(flatten)]
        apply: ApplyTiming,
    },
    /// The allowlist and the blocklist of addresses
    Set {
        #[command(subcommand)]
        action: FirewallSetAction,
    },
    /// Change firewall settings: inbound policy, ping, IPv6, Docker ports, presets
    Settings(SettingsArgs),
    /// Show the table the next apply would load
    Render,
    /// Load the stored rules, with an automatic rollback unless confirmed
    Apply(ApplyArgs),
    /// Confirm the change that is waiting
    Confirm,
    /// Undo the change that is waiting
    Rollback,
    /// The whole ruleset: show it, edit it as text, or replace it from a file
    Ruleset {
        #[command(subcommand)]
        action: RulesetAction,
    },
    /// Every nftables table on the host, read-only
    Tables,
}

#[derive(clap::Args)]
pub struct ApplyArgs {
    /// Seconds to confirm before the previous rules come back (15-600)
    #[arg(long, value_name = "SECONDS", default_value_t = 60)]
    timeout: u64,
    /// Commit at once, with no rollback window (for scripts)
    #[arg(long, conflicts_with = "timeout")]
    no_rollback: bool,
    /// Switch off a competing ufw or firewalld
    #[arg(long)]
    force: bool,
    /// Do not wait for the confirmation prompt
    #[arg(long)]
    no_wait: bool,
}

#[derive(clap::Args)]
pub struct RuleArgs {
    /// Ports and protocol: `443`, `80,443/tcp`, `8000-8100/udp`, `icmp`
    spec: Option<String>,
    /// Only for these sources: an IP, a CIDR, @allowlist or @blocklist. Repeatable
    #[arg(long = "from", value_name = "ADDRESS")]
    from: Vec<String>,
    /// host — traffic to this machine; docker — ports Docker publishes; both
    #[arg(long, value_enum, default_value_t = ScopeArg::Host)]
    scope: ScopeArg,
    /// A note shown next to the rule
    #[arg(long)]
    comment: Option<String>,
    /// Answer with a reset or an ICMP error instead of silently dropping (deny only)
    #[arg(long)]
    reject: bool,
    /// Rule id (default: generated)
    #[arg(long)]
    id: Option<String>,
    /// Store the rule without applying it
    #[arg(long)]
    no_apply: bool,
    #[command(flatten)]
    apply: ApplyTiming,
}

/// How a rule change that applies at once waits for confirmation.
#[derive(clap::Args)]
pub struct ApplyTiming {
    /// Seconds to confirm before the previous rules come back (15-600)
    #[arg(long = "timeout", value_name = "SECONDS", default_value_t = 60)]
    timeout: u64,
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum ScopeArg {
    Host,
    Docker,
    Both,
}

impl ScopeArg {
    fn label(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Docker => "docker",
            Self::Both => "both",
        }
    }
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum SetArg {
    Allowlist,
    Blocklist,
}

impl SetArg {
    fn label(self) -> &'static str {
        match self {
            Self::Allowlist => "allowlist",
            Self::Blocklist => "blocklist",
        }
    }
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum PolicyArg {
    Accept,
    Drop,
}

#[derive(Subcommand)]
pub enum FirewallSetAction {
    /// Show the entries of the allowlist and the blocklist
    List {
        /// Only this set
        #[arg(value_enum)]
        set: Option<SetArg>,
    },
    /// Add an address or network: `asc firewall set add blocklist 203.0.113.7 --ttl 24h`
    Add {
        #[arg(value_enum)]
        set: SetArg,
        /// An IP or a CIDR
        address: String,
        /// Drop the entry after this long: 90s, 30m, 24h, 7d
        #[arg(long)]
        ttl: Option<String>,
        #[arg(long)]
        comment: Option<String>,
        /// Store the entry without applying it
        #[arg(long)]
        no_apply: bool,
        #[command(flatten)]
        apply: ApplyTiming,
    },
    /// Remove an address or network
    Remove {
        #[arg(value_enum)]
        set: SetArg,
        address: String,
        /// Store the change without applying it
        #[arg(long)]
        no_apply: bool,
        #[command(flatten)]
        apply: ApplyTiming,
    },
}

#[derive(clap::Args)]
pub struct SettingsArgs {
    /// What happens to inbound traffic no rule allows
    #[arg(long, value_enum)]
    policy: Option<PolicyArg>,
    /// Answer ping
    #[arg(long, value_name = "BOOL")]
    icmp: Option<bool>,
    /// Filter IPv6 with the same rules (off leaves IPv6 unfiltered)
    #[arg(long, value_name = "BOOL")]
    ipv6: Option<bool>,
    /// Close the ports Docker publishes unless a rule opens them
    #[arg(long, value_name = "BOOL")]
    protect_docker: Option<bool>,
    /// Switch a preset off: ssh, api or web. Repeatable
    #[arg(long, value_name = "NAME")]
    disable_preset: Vec<String>,
    /// Switch a preset back on. Repeatable
    #[arg(long, value_name = "NAME")]
    enable_preset: Vec<String>,
    /// Needed to switch off the ssh or api preset, which can lock you out
    #[arg(long)]
    force: bool,
}

#[derive(Subcommand)]
pub enum RulesetAction {
    /// Print the ruleset as the kernel has it (or your RAW text with --raw)
    Show {
        #[arg(long)]
        raw: bool,
    },
    /// Edit the whole ruleset in $EDITOR and apply it (RAW mode)
    Edit {
        /// Confirm that this replaces every table and can lock you out
        #[arg(long)]
        i_understand_the_risk: bool,
        #[arg(long, value_name = "SECONDS", default_value_t = 60)]
        timeout: u64,
        #[arg(long)]
        no_wait: bool,
    },
    /// Replace the whole ruleset with a file's content (RAW mode); `-` reads stdin
    Apply {
        file: PathBuf,
        #[arg(long)]
        i_understand_the_risk: bool,
        #[arg(long, value_name = "SECONDS", default_value_t = 60)]
        timeout: u64,
        #[arg(long)]
        no_wait: bool,
    },
}

pub fn run(action: FirewallAction, config: &Config) -> anyhow::Result<()> {
    let Some(d) = daemon_backend(config)? else {
        anyhow::bail!("{}", t(Msg::FirewallDaemonRequired));
    };
    match action {
        FirewallAction::Status => status(&d),
        FirewallAction::Install => install(&d),
        FirewallAction::Enable(args) => {
            let overview = d.web("GET", "/v1/firewall", None)?;
            if !overview["nft_installed"].as_bool().unwrap_or(false) {
                anyhow::bail!("{}", t(Msg::FirewallNftMissing));
            }
            apply_flow(&d, &args)
        }
        FirewallAction::Apply(args) => apply_flow(&d, &args),
        FirewallAction::Disable => {
            d.web("POST", "/v1/firewall/disable", None)?;
            println!("{}", t(Msg::FirewallDisabled));
            Ok(())
        }
        FirewallAction::Allow(args) => rule(&d, args, "accept"),
        FirewallAction::Deny(args) => {
            let action = if args.reject { "reject" } else { "drop" };
            rule(&d, args, action)
        }
        FirewallAction::Rules => rules(&d),
        FirewallAction::Remove {
            id,
            no_apply,
            apply,
        } => {
            let removed = d.web("DELETE", &format!("/v1/firewall/rules/{id}"), None)?["removed"]
                .as_bool()
                .unwrap_or(false);
            if !removed {
                anyhow::bail!("{}", tf(Msg::FirewallRuleNotFound, &id));
            }
            println!("{}", tf(Msg::FirewallRuleRemoved, &id));
            apply_stored(&d, no_apply, apply.timeout)
        }
        FirewallAction::Set { action } => set(&d, action),
        FirewallAction::Settings(args) => settings(&d, args),
        FirewallAction::Render => {
            let json = d.web("GET", "/v1/firewall/render", None)?;
            print!("{}", json["proposed"].as_str().unwrap_or_default());
            if !json["changed"].as_bool().unwrap_or(false) {
                eprintln!("{}", t(Msg::FirewallRulesetUnchanged));
            }
            Ok(())
        }
        FirewallAction::Confirm => {
            d.web("POST", "/v1/firewall/confirm", Some(json!({})))?;
            println!("{}", t(Msg::FirewallConfirmed));
            Ok(())
        }
        FirewallAction::Rollback => {
            d.web("POST", "/v1/firewall/rollback", None)?;
            println!("{}", t(Msg::FirewallRolledBack));
            Ok(())
        }
        FirewallAction::Ruleset { action } => ruleset(&d, action),
        FirewallAction::Tables => tables(&d),
    }
}

// ── Reading ─────────────────────────────────────────────────────────────────

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or_default()
}

fn on_off(v: bool) -> &'static str {
    if v { "on" } else { "off" }
}

fn status(d: &Daemon) -> anyhow::Result<()> {
    let o = d.web("GET", "/v1/firewall", None)?;
    if !o["nft_installed"].as_bool().unwrap_or(false) {
        println!("{}", t(Msg::FirewallNftMissing));
        return Ok(());
    }
    println!(
        "{:<12} {} (nftables {})",
        "FIREWALL",
        text(&o, "mode"),
        text(&o, "nft_version")
    );
    let s = &o["settings"];
    println!(
        "{:<12} inbound {} · ping {} · IPv6 {} · Docker ports {}",
        "SETTINGS",
        text(s, "input_policy"),
        on_off(s["allow_icmp"].as_bool().unwrap_or(true)),
        if s["ipv6"].as_bool().unwrap_or(true) {
            "filtered"
        } else {
            "untouched"
        },
        if s["protect_docker"].as_bool().unwrap_or(false) {
            "closed unless allowed"
        } else {
            "open"
        },
    );
    let applied = o["applied_unix"].as_i64().unwrap_or(0);
    if applied > 0 {
        println!("{:<12} {}", "APPLIED", local_minute(applied));
    }
    if !o["pending"].is_null() {
        println!(
            "{:<12} {}",
            "PENDING",
            tf(
                Msg::FirewallPending,
                o["pending"]["seconds_left"].as_i64().unwrap_or(0)
            )
        );
    }
    for c in o["conflicts"].as_array().into_iter().flatten() {
        println!("{:<12} {}", "CONFLICT", text(c, "detail"));
    }
    let error = text(&o, "last_error");
    if !error.is_empty() {
        println!("{:<12} {}", "LAST ERROR", error.trim());
    }
    if text(&o, "mode") == "disabled" {
        println!("\n{}", t(Msg::FirewallOffNote));
    } else if o["changed"].as_bool().unwrap_or(false) {
        println!("\n{}", t(Msg::FirewallUnapplied));
    }
    println!();
    print_rules(&o);
    Ok(())
}

fn rules(d: &Daemon) -> anyhow::Result<()> {
    let o = d.web("GET", "/v1/firewall", None)?;
    print_rules(&o);
    Ok(())
}

fn print_rules(o: &Value) {
    let presets = o["presets"].as_array().cloned().unwrap_or_default();
    let user = o["rules"].as_array().cloned().unwrap_or_default();
    if presets.is_empty() && user.is_empty() {
        println!("{}", t(Msg::FirewallNoRules));
        return;
    }
    let id_w = presets
        .iter()
        .chain(&user)
        .map(|r| text(r, "id").len())
        .max()
        .unwrap_or(2)
        .max(2);
    println!(
        "{:<id_w$}  {:<6}  {:<5}  {:<5}  {:<14}  {:<22}  {:<7}  {:>8}  COMMENT",
        "ID", "ACTION", "STATE", "PROTO", "PORTS", "FROM", "SCOPE", "PACKETS"
    );
    for r in presets.iter().chain(&user) {
        let list = |key: &str, empty: &str| {
            let items: Vec<&str> = r[key]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            if items.is_empty() {
                empty.to_string()
            } else {
                items.join(",")
            }
        };
        let packets = o["counters"][text(r, "id")]["packets"]
            .as_u64()
            .map_or("-".to_string(), |p| p.to_string());
        println!(
            "{:<id_w$}  {:<6}  {:<5}  {:<5}  {:<14}  {:<22}  {:<7}  {:>8}  {}",
            text(r, "id"),
            text(r, "action"),
            if r["enabled"].as_bool().unwrap_or(true) {
                "on"
            } else {
                "off"
            },
            text(r, "protocol"),
            list("ports", "-"),
            list("sources", "any"),
            text(r, "scope"),
            packets,
            text(r, "comment"),
        );
    }
}

fn tables(d: &Daemon) -> anyhow::Result<()> {
    let json = d.web("GET", "/v1/firewall/tables", None)?;
    for table in json["tables"].as_array().into_iter().flatten() {
        println!(
            "# {} {} ({})",
            text(table, "family"),
            text(table, "name"),
            text(table, "owner")
        );
        print!("{}", text(table, "text"));
        println!();
    }
    Ok(())
}

// ── Installing and applying ─────────────────────────────────────────────────

fn install(d: &Daemon) -> anyhow::Result<()> {
    eprintln!("{}", t(Msg::FirewallInstalling));
    let json = d.web("POST", "/v1/firewall/install", None)?;
    for line in json["log"].as_array().into_iter().flatten() {
        eprintln!("  {}", line.as_str().unwrap_or_default());
    }
    println!(
        "{}",
        tf(
            Msg::FirewallInstalled,
            json["firewall"]["nft_version"].as_str().unwrap_or("")
        )
    );
    Ok(())
}

fn timeout_of(args: &ApplyArgs) -> u64 {
    if args.no_rollback { 0 } else { args.timeout }
}

fn apply_flow(d: &Daemon, args: &ApplyArgs) -> anyhow::Result<()> {
    let overview = d.web(
        "POST",
        "/v1/firewall/apply",
        Some(json!({ "timeout_seconds": timeout_of(args), "force": args.force })),
    )?;
    after_apply(d, &overview, !args.no_wait)
}

/// Applies what is stored (after a rule or set change) unless `no_apply`;
/// with the firewall off there is nothing to apply and the change just waits.
fn apply_stored(d: &Daemon, no_apply: bool, timeout: u64) -> anyhow::Result<()> {
    if no_apply {
        return Ok(());
    }
    let state = d.web("GET", "/v1/firewall", None)?;
    if text(&state, "mode") != "managed" {
        println!("{}", t(Msg::FirewallStoredNote));
        return Ok(());
    }
    let overview = d.web(
        "POST",
        "/v1/firewall/apply",
        Some(json!({ "timeout_seconds": timeout })),
    )?;
    after_apply(d, &overview, true)
}

/// Reports an apply and, while a rollback timer runs, asks for confirmation
/// on a terminal or says how to confirm elsewhere.
fn after_apply(d: &Daemon, overview: &Value, wait: bool) -> anyhow::Result<()> {
    println!("{}", t(Msg::FirewallApplied));
    let pending = &overview["pending"];
    if pending.is_null() {
        return Ok(());
    }
    let seconds = pending["seconds_left"].as_u64().unwrap_or(0);
    let id = text(pending, "id").to_string();
    if !wait || !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        println!("{}", tf(Msg::FirewallConfirmHint, seconds));
        return Ok(());
    }
    eprint!("{} ", tf(Msg::FirewallConfirmPrompt, seconds));
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        let _ = tx.send(line);
    });
    match rx.recv_timeout(Duration::from_secs(seconds.saturating_sub(2).max(1))) {
        Ok(answer) => {
            if matches!(
                answer.trim().to_ascii_lowercase().as_str(),
                "" | "y" | "yes" | "д" | "да"
            ) {
                d.web("POST", "/v1/firewall/confirm", Some(json!({ "id": id })))?;
                println!("{}", t(Msg::FirewallConfirmed));
            } else {
                d.web("POST", "/v1/firewall/rollback", None)?;
                println!("{}", t(Msg::FirewallRolledBack));
            }
        }
        Err(_) => {
            eprintln!();
            println!("{}", t(Msg::FirewallNoAnswer));
        }
    }
    Ok(())
}

// ── Rules ───────────────────────────────────────────────────────────────────

/// `443`, `80,443/tcp`, `53/udp`, `icmp`, `tcp` → protocol and ports.
/// Ports alone mean TCP.
pub fn parse_spec(spec: Option<&str>) -> anyhow::Result<(&'static str, Vec<String>)> {
    let Some(spec) = spec.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(("any", Vec::new()));
    };
    let bad = || anyhow::anyhow!("{}", tf(Msg::FirewallBadSpec, spec));
    match spec.to_ascii_lowercase().as_str() {
        "icmp" => return Ok(("icmp", Vec::new())),
        "tcp" => return Ok(("tcp", Vec::new())),
        "udp" => return Ok(("udp", Vec::new())),
        _ => {}
    }
    let (ports, proto) = match spec.rsplit_once('/') {
        Some((ports, proto)) => (ports, proto.to_ascii_lowercase()),
        None => (spec, "tcp".to_string()),
    };
    let proto = match proto.as_str() {
        "tcp" => "tcp",
        "udp" => "udp",
        _ => return Err(bad()),
    };
    let ports: Vec<String> = ports
        .split(',')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    if ports.is_empty()
        || ports
            .iter()
            .any(|p| !p.chars().all(|c| c.is_ascii_digit() || c == '-'))
    {
        return Err(bad());
    }
    Ok((proto, ports))
}

/// `90s`, `30m`, `24h`, `7d` or plain seconds → seconds.
pub fn parse_ttl(text: &str) -> anyhow::Result<i64> {
    let bad = || anyhow::anyhow!("{}", tf(Msg::FirewallBadTtl, text));
    let text = text.trim();
    let (number, unit) = match text.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((at, _)) => text.split_at(at),
        None => (text, "s"),
    };
    let value: i64 = number.parse().map_err(|_| bad())?;
    let factor = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return Err(bad()),
    };
    if value <= 0 {
        return Err(bad());
    }
    value.checked_mul(factor).ok_or_else(bad)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn rule(d: &Daemon, args: RuleArgs, action: &str) -> anyhow::Result<()> {
    let (protocol, ports) = parse_spec(args.spec.as_deref())?;
    if ports.is_empty() && protocol == "any" && args.from.is_empty() {
        anyhow::bail!("{}", t(Msg::FirewallRuleNeedsTarget));
    }
    let body = json!({
        "id": args.id.clone().unwrap_or_default(),
        "enabled": true,
        "action": action,
        "protocol": protocol,
        "ports": ports,
        "sources": args.from,
        "scope": args.scope.label(),
        "comment": args.comment.clone().unwrap_or_default(),
    });
    let saved = d.web("POST", "/v1/firewall/rules", Some(body))?;
    println!("{}", tf(Msg::FirewallRuleSaved, text(&saved, "id")));
    apply_stored(d, args.no_apply, args.apply.timeout)
}

// ── Sets ────────────────────────────────────────────────────────────────────

fn set(d: &Daemon, action: FirewallSetAction) -> anyhow::Result<()> {
    match action {
        FirewallSetAction::List { set } => {
            let o = d.web("GET", "/v1/firewall", None)?;
            let sets: Vec<SetArg> = match set {
                Some(one) => vec![one],
                None => vec![SetArg::Allowlist, SetArg::Blocklist],
            };
            for which in sets {
                let entries = o["sets"][which.label()]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                if entries.is_empty() {
                    println!("{}", tf(Msg::FirewallSetEmpty, which.label()));
                    continue;
                }
                println!("# {}", which.label());
                println!("{:<40}  {:<16}  COMMENT", "ADDRESS", "EXPIRES");
                for e in &entries {
                    println!(
                        "{:<40}  {:<16}  {}",
                        text(e, "value"),
                        local_minute(e["expires_unix"].as_i64().unwrap_or(0)),
                        text(e, "comment")
                    );
                }
            }
            Ok(())
        }
        FirewallSetAction::Add {
            set,
            address,
            ttl,
            comment,
            no_apply,
            apply,
        } => {
            let expires = match ttl {
                Some(ttl) => Some(now_unix() + parse_ttl(&ttl)?),
                None => None,
            };
            d.web(
                "POST",
                &format!("/v1/firewall/sets/{}/entries", set.label()),
                Some(json!({
                    "value": address,
                    "expires_unix": expires,
                    "comment": comment.unwrap_or_default(),
                })),
            )?;
            println!("{}", tf2(Msg::FirewallSetAdded, &address, set.label()));
            apply_stored(d, no_apply, apply.timeout)
        }
        FirewallSetAction::Remove {
            set,
            address,
            no_apply,
            apply,
        } => {
            let removed = d.web(
                "DELETE",
                &format!(
                    "/v1/firewall/sets/{}/entries?value={}",
                    set.label(),
                    percent_encode(&address)
                ),
                None,
            )?["removed"]
                .as_bool()
                .unwrap_or(false);
            if !removed {
                anyhow::bail!("{}", tf2(Msg::FirewallSetNotFound, &address, set.label()));
            }
            println!("{}", tf2(Msg::FirewallSetRemoved, &address, set.label()));
            apply_stored(d, no_apply, apply.timeout)
        }
    }
}

/// Just enough percent-encoding for an IP or a CIDR in a query string.
fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'.' | b':' | b'-' | b'_' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

// ── Settings ────────────────────────────────────────────────────────────────

fn settings(d: &Daemon, args: SettingsArgs) -> anyhow::Result<()> {
    let current = d.web("GET", "/v1/firewall", None)?["settings"].clone();
    let mut next = current.clone();
    if let Some(policy) = args.policy {
        next["input_policy"] = json!(match policy {
            PolicyArg::Accept => "accept",
            PolicyArg::Drop => "drop",
        });
    }
    if let Some(v) = args.icmp {
        next["allow_icmp"] = json!(v);
    }
    if let Some(v) = args.ipv6 {
        next["ipv6"] = json!(v);
    }
    if let Some(v) = args.protect_docker {
        next["protect_docker"] = json!(v);
    }
    let mut disabled: Vec<String> = current["disabled_presets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    for name in &args.enable_preset {
        disabled.retain(|p| p != name);
    }
    for name in args.disable_preset {
        if !disabled.contains(&name) {
            disabled.push(name);
        }
    }
    next["disabled_presets"] = json!(disabled);
    d.web(
        "PUT",
        "/v1/firewall/settings",
        Some(json!({ "settings": next, "force": args.force })),
    )?;
    println!("{}", t(Msg::FirewallSettingsSaved));
    apply_stored(d, false, 60)
}

// ── RAW ruleset ─────────────────────────────────────────────────────────────

fn ruleset(d: &Daemon, action: RulesetAction) -> anyhow::Result<()> {
    match action {
        RulesetAction::Show { raw } => {
            let json = d.web("GET", "/v1/firewall/ruleset", None)?;
            let key = if raw { "raw" } else { "live" };
            print!("{}", text(&json, key));
            Ok(())
        }
        RulesetAction::Edit {
            i_understand_the_risk,
            timeout,
            no_wait,
        } => {
            if !i_understand_the_risk {
                anyhow::bail!("{}", t(Msg::FirewallRiskRequired));
            }
            let current = d.web("GET", "/v1/firewall/ruleset", None)?;
            let state = d.web("GET", "/v1/firewall", None)?;
            let start = if text(&state, "mode") == "raw" && !text(&current, "raw").is_empty() {
                text(&current, "raw")
            } else {
                text(&current, "live")
            };
            let edited = edit_in_editor(start)?;
            if edited.trim() == start.trim() {
                println!("{}", t(Msg::FirewallRulesetUnchanged));
                return Ok(());
            }
            apply_ruleset(d, &edited, timeout, !no_wait)
        }
        RulesetAction::Apply {
            file,
            i_understand_the_risk,
            timeout,
            no_wait,
        } => {
            if !i_understand_the_risk {
                anyhow::bail!("{}", t(Msg::FirewallRiskRequired));
            }
            let body = if file.as_os_str() == "-" {
                let mut buf = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
                buf
            } else {
                std::fs::read_to_string(&file)
                    .map_err(|err| anyhow::anyhow!("cannot read {}: {err}", file.display()))?
            };
            apply_ruleset(d, &body, timeout, !no_wait)
        }
    }
}

fn apply_ruleset(d: &Daemon, text: &str, timeout: u64, wait: bool) -> anyhow::Result<()> {
    let overview = d.web(
        "PUT",
        "/v1/firewall/ruleset",
        Some(json!({
            "ruleset": text,
            "acknowledge_risk": true,
            "timeout_seconds": timeout,
        })),
    )?;
    after_apply(d, &overview, wait)
}

/// Opens `content` in `$VISUAL` / `$EDITOR` (default `vi`) and returns what
/// was saved.
fn edit_in_editor(content: &str) -> anyhow::Result<String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let path = std::env::temp_dir().join(format!("asc-ruleset-{}.nft", std::process::id()));
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|err| anyhow::anyhow!("cannot create {}: {err}", path.display()))?;
        file.write_all(content.as_bytes())?;
    }
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    let mut words = editor.split_whitespace();
    let program = words.next().unwrap_or("vi");
    let status = std::process::Command::new(program)
        .args(words)
        .arg(&path)
        .status();
    let result = match status {
        Ok(status) if status.success() => std::fs::read_to_string(&path)
            .map_err(|err| anyhow::anyhow!("cannot read {}: {err}", path.display())),
        Ok(status) => Err(anyhow::anyhow!("{editor} exited with {status}")),
        Err(err) => Err(anyhow::anyhow!("cannot start {editor}: {err}")),
    };
    let _ = std::fs::remove_file(&path);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_default_to_tcp_and_split_ports() {
        assert_eq!(parse_spec(None).unwrap(), ("any", vec![]));
        assert_eq!(
            parse_spec(Some("443")).unwrap(),
            ("tcp", vec!["443".into()])
        );
        assert_eq!(
            parse_spec(Some("80,443/tcp")).unwrap(),
            ("tcp", vec!["80".into(), "443".into()])
        );
        assert_eq!(
            parse_spec(Some("8000-8100/UDP")).unwrap(),
            ("udp", vec!["8000-8100".into()])
        );
        assert_eq!(parse_spec(Some("icmp")).unwrap(), ("icmp", vec![]));
        assert_eq!(parse_spec(Some("tcp")).unwrap(), ("tcp", vec![]));
    }

    #[test]
    fn bad_specs_are_refused() {
        for bad in ["80/sctp", "http", "80;drop", "/tcp", "80/"] {
            assert!(parse_spec(Some(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn ttls_have_units() {
        assert_eq!(parse_ttl("90s").unwrap(), 90);
        assert_eq!(parse_ttl("30m").unwrap(), 1800);
        assert_eq!(parse_ttl("24h").unwrap(), 86_400);
        assert_eq!(parse_ttl("7d").unwrap(), 604_800);
        assert_eq!(parse_ttl("120").unwrap(), 120);
        for bad in ["", "h", "0s", "-5m", "5x", "1.5h"] {
            assert!(parse_ttl(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn query_values_are_percent_encoded() {
        assert_eq!(percent_encode("10.0.0.0/8"), "10.0.0.0%2F8");
        assert_eq!(percent_encode("2001:db8::/32"), "2001:db8::%2F32");
    }
}
