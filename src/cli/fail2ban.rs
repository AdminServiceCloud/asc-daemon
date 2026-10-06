//! `asc fail2ban`: the node's fail2ban through the running daemon.

use clap::Subcommand;
use serde_json::{Value, json};

use asc_daemon::daemon::client::Daemon;
use asc_daemon::daemon::config::Config;
use asc_daemon::daemon::i18n::{Msg, t, tf, tf2};

use super::{daemon_backend, local_minute};

#[derive(Subcommand)]
pub enum Fail2banAction {
    /// Show fail2ban: version, state, defaults and the jails with their counters
    Status,
    /// Install fail2ban and write the daemon's configuration (jail.d/asc.local)
    Install,
    /// Remove fail2ban
    Uninstall {
        /// Also delete the daemon's state and configuration
        #[arg(long)]
        purge: bool,
    },
    /// Change the defaults: ban time, find window, retries, ignored addresses
    Settings(SettingsArgs),
    /// List the jails the daemon can run
    Jails,
    /// Switch a jail on: `asc fail2ban enable sshd`
    Enable { jail: String },
    /// Switch a jail off
    Disable { jail: String },
    /// Override a jail's retries, times, port or log path
    Tune(TuneArgs),
    /// List banned addresses
    Bans {
        /// Only this jail
        #[arg(long)]
        jail: Option<String>,
    },
    /// Ban an address by hand
    Ban {
        /// One IP address
        ip: String,
        #[arg(long, default_value = "sshd")]
        jail: String,
    },
    /// Release an address (from every jail unless --jail is given)
    Unban {
        ip: String,
        #[arg(long)]
        jail: Option<String>,
    },
}

#[derive(clap::Args)]
pub struct SettingsArgs {
    /// How long a ban lasts: 90s, 10m, 1h, 1d, 1w, or -1 for good
    #[arg(long)]
    bantime: Option<String>,
    /// The window in which failures are counted
    #[arg(long)]
    findtime: Option<String>,
    /// Failures inside the window that cause a ban
    #[arg(long)]
    maxretry: Option<u32>,
    /// Ban repeat offenders for longer each time
    #[arg(long, value_name = "BOOL")]
    increment: Option<bool>,
    /// Never ban this IP or CIDR. Repeatable
    #[arg(long, value_name = "ADDRESS")]
    add_ignore: Vec<String>,
    /// Stop ignoring this IP or CIDR. Repeatable
    #[arg(long, value_name = "ADDRESS")]
    remove_ignore: Vec<String>,
}

#[derive(clap::Args)]
pub struct TuneArgs {
    /// sshd, recidive, nginx-http-auth, nginx-botsearch or nginx-limit-req
    jail: String,
    #[arg(long)]
    maxretry: Option<u32>,
    #[arg(long)]
    bantime: Option<String>,
    #[arg(long)]
    findtime: Option<String>,
    /// Ports, ranges or service names: 22, 22,2222, http,https
    #[arg(long)]
    port: Option<String>,
    /// Absolute log path (globs allowed)
    #[arg(long)]
    logpath: Option<String>,
}

pub fn run(action: Fail2banAction, config: &Config) -> anyhow::Result<()> {
    let Some(d) = daemon_backend(config)? else {
        anyhow::bail!("{}", t(Msg::Fail2banDaemonRequired));
    };
    match action {
        Fail2banAction::Status => status(&d),
        Fail2banAction::Install => {
            eprintln!("{}", t(Msg::Fail2banInstalling));
            let json = d.web("POST", "/v1/fail2ban/install", None)?;
            for line in json["log"].as_array().into_iter().flatten() {
                eprintln!("  {}", line.as_str().unwrap_or_default());
            }
            println!(
                "{}",
                tf(
                    Msg::Fail2banInstalled,
                    json["fail2ban"]["version"].as_str().unwrap_or("")
                )
            );
            Ok(())
        }
        Fail2banAction::Uninstall { purge } => {
            let json = d.web("DELETE", &format!("/v1/fail2ban?purge={purge}"), None)?;
            for line in json["log"].as_array().into_iter().flatten() {
                eprintln!("  {}", line.as_str().unwrap_or_default());
            }
            println!("{}", t(Msg::Fail2banUninstalled));
            Ok(())
        }
        Fail2banAction::Settings(args) => settings(&d, args),
        Fail2banAction::Jails => {
            let o = overview(&d)?;
            print_jails(&o);
            Ok(())
        }
        Fail2banAction::Enable { jail } => set_enabled(&d, &jail, true),
        Fail2banAction::Disable { jail } => set_enabled(&d, &jail, false),
        Fail2banAction::Tune(args) => tune(&d, args),
        Fail2banAction::Bans { jail } => bans(&d, jail.as_deref()),
        Fail2banAction::Ban { ip, jail } => {
            d.web(
                "POST",
                "/v1/fail2ban/bans",
                Some(json!({ "jail": jail, "ip": ip })),
            )?;
            println!("{}", tf2(Msg::Fail2banBanned, &ip, &jail));
            Ok(())
        }
        Fail2banAction::Unban { ip, jail } => {
            let released = d.web(
                "POST",
                "/v1/fail2ban/unban",
                Some(json!({ "jail": jail.unwrap_or_default(), "ip": ip })),
            )?["released"]
                .as_bool()
                .unwrap_or(false);
            if released {
                println!("{}", tf(Msg::Fail2banUnbanned, &ip));
            } else {
                println!("{}", tf(Msg::Fail2banNotBanned, &ip));
            }
            Ok(())
        }
    }
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or_default()
}

/// The overview, or a hint when fail2ban is not installed.
fn overview(d: &Daemon) -> anyhow::Result<Value> {
    let o = d.web("GET", "/v1/fail2ban", None)?;
    if !o["installed"].as_bool().unwrap_or(false) {
        anyhow::bail!("{}", t(Msg::Fail2banNotInstalled));
    }
    Ok(o)
}

fn status(d: &Daemon) -> anyhow::Result<()> {
    let o = d.web("GET", "/v1/fail2ban", None)?;
    if !o["installed"].as_bool().unwrap_or(false) {
        println!("{}", t(Msg::Fail2banNotInstalled));
        return Ok(());
    }
    println!(
        "{:<12} fail2ban {} ({})",
        "FAIL2BAN",
        text(&o, "version"),
        if o["running"].as_bool().unwrap_or(false) {
            "running"
        } else {
            "stopped"
        }
    );
    let s = &o["settings"];
    let ignore: Vec<&str> = s["ignoreip"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    println!(
        "{:<12} ban {} · window {} · {} failures · increment {} · ignore {}",
        "DEFAULTS",
        text(s, "bantime"),
        text(s, "findtime"),
        s["maxretry"],
        if s["bantime_increment"].as_bool().unwrap_or(false) {
            "on"
        } else {
            "off"
        },
        if ignore.is_empty() {
            "-".to_string()
        } else {
            ignore.join(",")
        }
    );
    let error = text(&o, "last_error");
    if !error.is_empty() {
        println!("{:<12} {}", "LAST ERROR", error.trim());
    }
    println!();
    print_jails(&o);
    Ok(())
}

fn print_jails(o: &Value) {
    println!(
        "{:<18}  {:<8}  {:<8}  {:>7}  {:>6}  {:>9}  {:>6}  NOTE",
        "JAIL", "ENABLED", "RUNNING", "FAILED", "BANNED", "TOTAL", "RETRY"
    );
    for j in o["jails"].as_array().into_iter().flatten() {
        let status = &j["status"];
        let number = |key: &str| {
            if status.is_null() {
                "-".to_string()
            } else {
                status[key].as_u64().unwrap_or(0).to_string()
            }
        };
        let note = if j["available"].as_bool().unwrap_or(true) {
            String::new()
        } else {
            "needs the web server".to_string()
        };
        println!(
            "{:<18}  {:<8}  {:<8}  {:>7}  {:>6}  {:>9}  {:>6}  {}",
            text(j, "name"),
            if j["enabled"].as_bool().unwrap_or(false) {
                "on"
            } else {
                "off"
            },
            if j["active"].as_bool().unwrap_or(false) {
                "yes"
            } else {
                "no"
            },
            number("currently_failed"),
            number("currently_banned"),
            number("total_banned"),
            j["maxretry"]
                .as_u64()
                .map_or("default".to_string(), |n| n.to_string()),
            note,
        );
    }
}

fn settings(d: &Daemon, args: SettingsArgs) -> anyhow::Result<()> {
    if args.bantime.is_none()
        && args.findtime.is_none()
        && args.maxretry.is_none()
        && args.increment.is_none()
        && args.add_ignore.is_empty()
        && args.remove_ignore.is_empty()
    {
        anyhow::bail!("{}", t(Msg::Fail2banNothingToChange));
    }
    let mut s = overview(d)?["settings"].clone();
    if let Some(v) = args.bantime {
        s["bantime"] = json!(v);
    }
    if let Some(v) = args.findtime {
        s["findtime"] = json!(v);
    }
    if let Some(v) = args.maxretry {
        s["maxretry"] = json!(v);
    }
    if let Some(v) = args.increment {
        s["bantime_increment"] = json!(v);
    }
    let mut ignore: Vec<String> = s["ignoreip"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    ignore.retain(|a| !args.remove_ignore.contains(a));
    for a in args.add_ignore {
        if !ignore.contains(&a) {
            ignore.push(a);
        }
    }
    s["ignoreip"] = json!(ignore);
    d.web("PUT", "/v1/fail2ban/settings", Some(s))?;
    println!("{}", t(Msg::Fail2banSettingsSaved));
    Ok(())
}

/// Fetches a jail's current configuration, lets `edit` change it and stores it.
fn update_jail(d: &Daemon, name: &str, edit: impl FnOnce(&mut Value)) -> anyhow::Result<bool> {
    let o = overview(d)?;
    let Some(current) = o["jails"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|j| text(j, "name") == name)
    else {
        anyhow::bail!(
            "unknown jail '{name}' (known: {})",
            o["jails"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|j| text(j, "name"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    };
    let mut body = json!({
        "name": name,
        "enabled": current["enabled"],
        "maxretry": current["maxretry"],
        "bantime": current["bantime"],
        "findtime": current["findtime"],
        "port": current["port"],
        "logpath": current["logpath"],
    });
    edit(&mut body);
    let saved = d.web("PUT", &format!("/v1/fail2ban/jails/{name}"), Some(body))?;
    Ok(saved["jails"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|j| text(j, "name") == name)
        .is_some_and(|j| j["enabled"].as_bool().unwrap_or(false)))
}

fn set_enabled(d: &Daemon, name: &str, enabled: bool) -> anyhow::Result<()> {
    let now = update_jail(d, name, |body| body["enabled"] = json!(enabled))?;
    println!(
        "{}",
        tf2(Msg::Fail2banJailSaved, name, if now { "on" } else { "off" })
    );
    Ok(())
}

fn tune(d: &Daemon, args: TuneArgs) -> anyhow::Result<()> {
    if args.maxretry.is_none()
        && args.bantime.is_none()
        && args.findtime.is_none()
        && args.port.is_none()
        && args.logpath.is_none()
    {
        anyhow::bail!("{}", t(Msg::Fail2banNothingToChange));
    }
    let now = update_jail(d, &args.jail, |body| {
        if let Some(v) = args.maxretry {
            body["maxretry"] = json!(v);
        }
        if let Some(v) = &args.bantime {
            body["bantime"] = json!(v);
        }
        if let Some(v) = &args.findtime {
            body["findtime"] = json!(v);
        }
        if let Some(v) = &args.port {
            body["port"] = json!(v);
        }
        if let Some(v) = &args.logpath {
            body["logpath"] = json!(v);
        }
    })?;
    println!(
        "{}",
        tf2(
            Msg::Fail2banJailSaved,
            &args.jail,
            if now { "on" } else { "off" }
        )
    );
    Ok(())
}

fn bans(d: &Daemon, jail: Option<&str>) -> anyhow::Result<()> {
    overview(d)?;
    let path = match jail {
        Some(j) => format!("/v1/fail2ban/bans?jail={j}"),
        None => "/v1/fail2ban/bans".to_string(),
    };
    let json = d.web("GET", &path, None)?;
    let bans = json["bans"].as_array().cloned().unwrap_or_default();
    if bans.is_empty() {
        println!("{}", t(Msg::Fail2banNoBans));
        return Ok(());
    }
    println!(
        "{:<40}  {:<16}  {:<16}  {:<16}",
        "ADDRESS", "JAIL", "BANNED", "UNTIL"
    );
    for b in &bans {
        let until = match b["unban_unix"].as_i64().unwrap_or(0) {
            0 => "-".to_string(),
            at => local_minute(at),
        };
        println!(
            "{:<40}  {:<16}  {:<16}  {:<16}",
            text(b, "ip"),
            text(b, "jail"),
            local_minute(b["banned_unix"].as_i64().unwrap_or(0)),
            until
        );
    }
    Ok(())
}
