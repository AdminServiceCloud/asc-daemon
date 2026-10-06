//! Talking to fail2ban through its own `fail2ban-client`. Behind a
//! trait so the manager is testable without fail2ban; the parsers turn the
//! client's human-oriented output into typed values and fail loudly on a
//! format they do not recognise instead of reporting an empty list.

use anyhow::{Result, bail};
use serde::Serialize;

use crate::daemon::exec::{has_command, run_captured};

/// What `fail2ban-client status <jail>` reports.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct JailStatus {
    pub currently_failed: u64,
    pub total_failed: u64,
    pub currently_banned: u64,
    pub total_banned: u64,
    pub banned_ips: Vec<String>,
}

/// One banned address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Ban {
    pub ip: String,
    pub jail: String,
    /// Unix time of the ban; 0 when unknown.
    pub banned_unix: i64,
    /// Unix time the ban ends; 0 when permanent or unknown.
    pub unban_unix: i64,
}

pub trait Client: Send + Sync {
    fn installed(&self) -> bool;
    fn version(&self) -> String;
    fn running(&self) -> bool;
    /// Enables the service and starts it (`systemctl enable --now`).
    fn start(&self) -> Result<()>;
    /// `fail2ban-client -t`: check the configuration.
    fn test_config(&self) -> Result<()>;
    /// `fail2ban-client reload`.
    fn reload(&self) -> Result<()>;
    /// `systemctl restart fail2ban`, then waits until the server answers.
    fn restart(&self) -> Result<()>;
    /// The ban actions the server attached to a running jail.
    fn jail_actions(&self, jail: &str) -> Result<Vec<String>>;
    /// Names of the jails the server runs.
    fn jails(&self) -> Result<Vec<String>>;
    fn jail_status(&self, jail: &str) -> Result<JailStatus>;
    fn bans(&self, jail: &str) -> Result<Vec<Ban>>;
    fn ban(&self, jail: &str, ip: &str) -> Result<()>;
    /// Releases `ip` from `jail`, or from every jail when `jail` is empty.
    fn unban(&self, jail: &str, ip: &str) -> Result<bool>;
}

pub struct SystemClient;

impl SystemClient {
    fn run(&self, args: &[&str]) -> Result<(bool, String)> {
        run_captured("fail2ban-client", args)
    }

    fn ok(&self, args: &[&str]) -> Result<String> {
        let (ok, out) = self.run(args)?;
        if !ok {
            bail!("fail2ban-client {} failed: {}", args.join(" "), out.trim());
        }
        Ok(out)
    }
}

impl Client for SystemClient {
    fn installed(&self) -> bool {
        has_command("fail2ban-client") || std::path::Path::new("/usr/bin/fail2ban-client").exists()
    }

    fn version(&self) -> String {
        self.run(&["--version"])
            .ok()
            .filter(|(ok, _)| *ok)
            .map(|(_, out)| parse_version(&out))
            .unwrap_or_default()
    }

    fn running(&self) -> bool {
        self.run(&["ping"])
            .map(|(ok, out)| ok && out.contains("pong"))
            .unwrap_or(false)
    }

    fn start(&self) -> Result<()> {
        let (ok, out) = run_captured("systemctl", &["enable", "--now", "fail2ban"])?;
        if !ok {
            bail!("cannot start fail2ban: {}", out.trim());
        }
        Ok(())
    }

    fn test_config(&self) -> Result<()> {
        let (ok, out) = self.run(&["-t"])?;
        if !ok {
            bail!("fail2ban rejected the configuration: {}", out.trim());
        }
        Ok(())
    }

    fn reload(&self) -> Result<()> {
        self.ok(&["reload"]).map(|_| ())
    }

    fn restart(&self) -> Result<()> {
        let (ok, out) = run_captured("systemctl", &["restart", "fail2ban"])?;
        if !ok {
            bail!("cannot restart fail2ban: {}", out.trim());
        }
        // `restart` returns when the process is up, not when the server
        // answers; the jails start a moment later.
        for _ in 0..30 {
            if self.running() {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        bail!("fail2ban did not answer after a restart")
    }

    fn jail_actions(&self, jail: &str) -> Result<Vec<String>> {
        Ok(parse_actions(&self.ok(&["get", jail, "actions"])?))
    }

    fn jails(&self) -> Result<Vec<String>> {
        parse_jail_list(&self.ok(&["status"])?)
    }

    fn jail_status(&self, jail: &str) -> Result<JailStatus> {
        parse_jail_status(&self.ok(&["status", jail])?)
    }

    fn bans(&self, jail: &str) -> Result<Vec<Ban>> {
        // `--with-time` exists since fail2ban 0.11; older servers only know
        // the plain list (no times).
        match self.run(&["get", jail, "banip", "--with-time"]) {
            Ok((true, out)) => Ok(parse_bans_with_time(&out, jail)),
            _ => Ok(self
                .jail_status(jail)?
                .banned_ips
                .into_iter()
                .map(|ip| Ban {
                    ip,
                    jail: jail.to_string(),
                    banned_unix: 0,
                    unban_unix: 0,
                })
                .collect()),
        }
    }

    fn ban(&self, jail: &str, ip: &str) -> Result<()> {
        self.ok(&["set", jail, "banip", ip]).map(|_| ())
    }

    fn unban(&self, jail: &str, ip: &str) -> Result<bool> {
        let out = if jail.is_empty() {
            self.ok(&["unban", ip])?
        } else {
            self.ok(&["set", jail, "unbanip", ip])?
        };
        // The client prints the number of released addresses (or the address).
        let released = out.trim();
        Ok(!(released == "0" || released.is_empty()))
    }
}

/// `Fail2Ban v1.0.2` / `1.1.0` → `1.0.2`.
pub fn parse_version(output: &str) -> String {
    output
        .split_whitespace()
        .map(|w| w.trim_start_matches('v'))
        .find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .map(|w| {
            w.chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect()
        })
        .unwrap_or_default()
}

/// The value after the last tab of a `|- Key:\tvalue` line.
fn value_of(line: &str) -> &str {
    line.rsplit_once('\t').map_or("", |(_, v)| v).trim()
}

/// `fail2ban-client get <jail> actions`:
///
/// ```text
/// The jail sshd has the following actions:
/// nftables-multiport
/// ```
///
/// or `No actions for jail sshd`.
pub fn parse_actions(output: &str) -> Vec<String> {
    if output.contains("No actions for jail") {
        return Vec::new();
    }
    output
        .lines()
        .skip(1)
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// `fail2ban-client status`:
///
/// ```text
/// Status
/// |- Number of jail:      2
/// `- Jail list:   recidive, sshd
/// ```
pub fn parse_jail_list(output: &str) -> Result<Vec<String>> {
    let Some(line) = output.lines().find(|l| l.contains("Jail list:")) else {
        bail!(
            "unexpected `fail2ban-client status` output: {}",
            output.trim()
        );
    };
    Ok(value_of(line)
        .split(',')
        .map(|j| j.trim().to_string())
        .filter(|j| !j.is_empty())
        .collect())
}

/// `fail2ban-client status <jail>`.
pub fn parse_jail_status(output: &str) -> Result<JailStatus> {
    if !output.contains("Currently banned") {
        bail!(
            "unexpected `fail2ban-client status <jail>` output: {}",
            output.trim()
        );
    }
    let mut status = JailStatus::default();
    for line in output.lines() {
        let number = || value_of(line).parse::<u64>().unwrap_or(0);
        if line.contains("Currently failed:") {
            status.currently_failed = number();
        } else if line.contains("Total failed:") {
            status.total_failed = number();
        } else if line.contains("Currently banned:") {
            status.currently_banned = number();
        } else if line.contains("Total banned:") {
            status.total_banned = number();
        } else if line.contains("Banned IP list:") {
            status.banned_ips = value_of(line)
                .split_whitespace()
                .map(str::to_string)
                .collect();
        }
    }
    Ok(status)
}

/// `get <jail> banip --with-time`, one address per line:
///
/// ```text
/// 1.2.3.4 \t2026-10-05 12:00:00 + 3600 = 2026-10-05 13:00:00
/// ```
pub fn parse_bans_with_time(output: &str, jail: &str) -> Vec<Ban> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let ip = line.split_whitespace().next()?;
            if ip.parse::<std::net::IpAddr>().is_err() {
                return None;
            }
            let rest = line[ip.len()..].trim();
            let (start, tail) = rest.split_once('+').unwrap_or((rest, ""));
            let (duration, end) = tail.split_once('=').unwrap_or(("", ""));
            let permanent = duration.trim().starts_with('-');
            Some(Ban {
                ip: ip.to_string(),
                jail: jail.to_string(),
                banned_unix: parse_local_time(start.trim()),
                unban_unix: if permanent {
                    0
                } else {
                    parse_local_time(end.trim())
                },
            })
        })
        .collect()
}

/// `2026-10-05 12:00:00` in the node's local time → unix time (0 on any
/// parse failure).
pub fn parse_local_time(text: &str) -> i64 {
    let mut parts = text.split(['-', ' ', ':']);
    let mut next = || parts.next().and_then(|p| p.trim().parse::<i32>().ok());
    let (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(s)) =
        (next(), next(), next(), next(), next(), next())
    else {
        return 0;
    };
    // SAFETY: mktime only reads and normalises the struct it is given.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = y - 1900;
    tm.tm_mon = mo - 1;
    tm.tm_mday = d;
    tm.tm_hour = h;
    tm.tm_min = mi;
    tm.tm_sec = s;
    tm.tm_isdst = -1;
    let t = unsafe { libc::mktime(&mut tm) };
    if t < 0 { 0 } else { t as i64 }
}

/// A scripted [`Client`] for tests.
#[cfg(test)]
pub mod fake {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use anyhow::{Result, bail};

    use super::{Ban, Client, JailStatus};

    #[derive(Default)]
    pub struct FakeClient {
        pub missing: bool,
        pub running: Mutex<bool>,
        pub reloads: Mutex<u32>,
        /// `test_config` fails while this is set.
        pub reject_config: Mutex<bool>,
        pub jails: Mutex<Vec<String>>,
        pub statuses: Mutex<BTreeMap<String, JailStatus>>,
        pub bans: Mutex<Vec<Ban>>,
        pub status_calls: Mutex<u32>,
        pub starts: Mutex<u32>,
        pub restarts: Mutex<u32>,
        /// A `reload` empties every jail's actions (the fail2ban quirk when a
        /// ban action is replaced) until the next `restart`.
        pub reload_drops_actions: Mutex<bool>,
        /// A `restart` does not bring the actions back either.
        pub restart_keeps_failing: Mutex<bool>,
        pub actions_lost: Mutex<bool>,
    }

    impl Client for FakeClient {
        fn installed(&self) -> bool {
            !self.missing
        }

        fn version(&self) -> String {
            "1.1.0".into()
        }

        fn running(&self) -> bool {
            *self.running.lock().unwrap()
        }

        fn start(&self) -> Result<()> {
            *self.running.lock().unwrap() = true;
            *self.starts.lock().unwrap() += 1;
            Ok(())
        }

        fn test_config(&self) -> Result<()> {
            if *self.reject_config.lock().unwrap() {
                bail!("fail2ban rejected the configuration: bad option");
            }
            Ok(())
        }

        fn reload(&self) -> Result<()> {
            *self.reloads.lock().unwrap() += 1;
            if *self.reload_drops_actions.lock().unwrap() {
                *self.actions_lost.lock().unwrap() = true;
            }
            Ok(())
        }

        fn restart(&self) -> Result<()> {
            *self.restarts.lock().unwrap() += 1;
            *self.running.lock().unwrap() = true;
            if !*self.restart_keeps_failing.lock().unwrap() {
                *self.actions_lost.lock().unwrap() = false;
            }
            Ok(())
        }

        fn jail_actions(&self, _jail: &str) -> Result<Vec<String>> {
            if *self.actions_lost.lock().unwrap() {
                Ok(Vec::new())
            } else {
                Ok(vec!["nftables-multiport".into()])
            }
        }

        fn jails(&self) -> Result<Vec<String>> {
            Ok(self.jails.lock().unwrap().clone())
        }

        fn jail_status(&self, jail: &str) -> Result<JailStatus> {
            *self.status_calls.lock().unwrap() += 1;
            Ok(self
                .statuses
                .lock()
                .unwrap()
                .get(jail)
                .cloned()
                .unwrap_or_default())
        }

        fn bans(&self, jail: &str) -> Result<Vec<Ban>> {
            Ok(self
                .bans
                .lock()
                .unwrap()
                .iter()
                .filter(|b| b.jail == jail)
                .cloned()
                .collect())
        }

        fn ban(&self, jail: &str, ip: &str) -> Result<()> {
            self.bans.lock().unwrap().push(Ban {
                ip: ip.into(),
                jail: jail.into(),
                banned_unix: 1,
                unban_unix: 0,
            });
            Ok(())
        }

        fn unban(&self, jail: &str, ip: &str) -> Result<bool> {
            let mut bans = self.bans.lock().unwrap();
            let before = bans.len();
            bans.retain(|b| !(b.ip == ip && (jail.is_empty() || b.jail == jail)));
            Ok(bans.len() != before)
        }
    }

    impl Client for Arc<FakeClient> {
        fn installed(&self) -> bool {
            (**self).installed()
        }
        fn version(&self) -> String {
            (**self).version()
        }
        fn running(&self) -> bool {
            (**self).running()
        }
        fn start(&self) -> Result<()> {
            (**self).start()
        }
        fn test_config(&self) -> Result<()> {
            (**self).test_config()
        }
        fn reload(&self) -> Result<()> {
            (**self).reload()
        }
        fn restart(&self) -> Result<()> {
            (**self).restart()
        }
        fn jail_actions(&self, jail: &str) -> Result<Vec<String>> {
            (**self).jail_actions(jail)
        }
        fn jails(&self) -> Result<Vec<String>> {
            (**self).jails()
        }
        fn jail_status(&self, jail: &str) -> Result<JailStatus> {
            (**self).jail_status(jail)
        }
        fn bans(&self, jail: &str) -> Result<Vec<Ban>> {
            (**self).bans(jail)
        }
        fn ban(&self, jail: &str, ip: &str) -> Result<()> {
            (**self).ban(jail, ip)
        }
        fn unban(&self, jail: &str, ip: &str) -> Result<bool> {
            (**self).unban(jail, ip)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_parsed() {
        assert_eq!(parse_version("Fail2Ban v1.0.2\nCopyright"), "1.0.2");
        assert_eq!(parse_version("1.1.0"), "1.1.0");
        assert_eq!(parse_version("garbage"), "");
    }

    #[test]
    fn jail_actions_are_parsed() {
        assert_eq!(
            parse_actions("The jail sshd has the following actions:\nnftables-multiport\n"),
            ["nftables-multiport"]
        );
        assert_eq!(
            parse_actions("The jail x has the following actions:\na\nb\n"),
            ["a", "b"]
        );
        assert!(parse_actions("No actions for jail sshd\n").is_empty());
    }

    #[test]
    fn the_jail_list_is_parsed() {
        let out = "Status\n|- Number of jail:\t2\n`- Jail list:\trecidive, sshd\n";
        assert_eq!(parse_jail_list(out).unwrap(), ["recidive", "sshd"]);
        let none = "Status\n|- Number of jail:\t0\n`- Jail list:\t\n";
        assert!(parse_jail_list(none).unwrap().is_empty());
        assert!(parse_jail_list("Failed to access socket path").is_err());
    }

    #[test]
    fn a_jail_status_is_parsed() {
        let out = "Status for the jail: sshd\n\
                   |- Filter\n\
                   |  |- Currently failed:\t2\n\
                   |  |- Total failed:\t17\n\
                   |  `- File list:\t/var/log/auth.log\n\
                   `- Actions\n   \
                   |- Currently banned:\t2\n   \
                   |- Total banned:\t5\n   \
                   `- Banned IP list:\t1.2.3.4 2001:db8::5\n";
        let status = parse_jail_status(out).unwrap();
        assert_eq!(status.currently_failed, 2);
        assert_eq!(status.total_failed, 17);
        assert_eq!(status.currently_banned, 2);
        assert_eq!(status.total_banned, 5);
        assert_eq!(status.banned_ips, ["1.2.3.4", "2001:db8::5"]);
    }

    #[test]
    fn an_empty_ban_list_and_unknown_output() {
        let out = "Status for the jail: sshd\n|- Filter\n|  |- Currently failed:\t0\n|  `- Total failed:\t0\n`- Actions\n   |- Currently banned:\t0\n   |- Total banned:\t0\n   `- Banned IP list:\t\n";
        assert!(parse_jail_status(out).unwrap().banned_ips.is_empty());
        assert!(parse_jail_status("Sorry but the jail 'x' does not exist").is_err());
    }

    #[test]
    fn bans_with_times_are_parsed() {
        let out = "1.2.3.4 \t2026-10-05 12:00:00 + 3600 = 2026-10-05 13:00:00\n\
                   2001:db8::7 \t2026-10-05 12:30:00 + -1 = 2026-10-05 12:30:00\n\
                   not an address\n";
        let bans = parse_bans_with_time(out, "sshd");
        assert_eq!(bans.len(), 2);
        assert_eq!(bans[0].ip, "1.2.3.4");
        assert_eq!(bans[0].jail, "sshd");
        assert_eq!(bans[0].unban_unix - bans[0].banned_unix, 3600);
        assert_eq!(bans[1].ip, "2001:db8::7");
        assert_eq!(bans[1].unban_unix, 0, "a permanent ban has no end");
    }

    #[test]
    fn bad_times_become_zero() {
        assert_eq!(parse_local_time("yesterday"), 0);
        assert_eq!(parse_local_time(""), 0);
        assert!(parse_local_time("2026-10-05 12:00:00") > 1_700_000_000);
    }
}
