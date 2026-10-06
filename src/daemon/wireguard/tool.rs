//! Talking to WireGuard: the `wg` and `wg-quick` tools and the
//! systemd units that keep an interface up. Behind a trait so the manager is
//! testable without a kernel module; the parser of `wg show … dump` fails
//! loudly on a shape it does not know.

use std::path::Path;

use anyhow::{Result, bail};

use crate::daemon::exec::{has_command, run_captured, run_with_input};

/// What a running interface reports about one peer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LivePeer {
    pub public_key: String,
    /// The address the peer was last seen at; empty before the first packet.
    pub endpoint: String,
    /// Unix time of the last handshake; 0 when there was none.
    pub latest_handshake_unix: i64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

pub trait Wg: Send + Sync {
    fn installed(&self) -> bool;
    fn version(&self) -> String;
    fn genkey(&self) -> Result<String>;
    fn pubkey(&self, private_key: &str) -> Result<String>;
    fn genpsk(&self) -> Result<String>;
    /// The interfaces that exist now (`wg show interfaces`).
    fn running(&self) -> Result<Vec<String>>;
    /// Live data of one interface's peers.
    fn live_peers(&self, interface: &str) -> Result<Vec<LivePeer>>;
    /// `wg-quick strip <file>`: the file without what only `wg-quick`
    /// understands. Also the check that the file is well formed.
    fn strip(&self, conf: &Path) -> Result<String>;
    /// `wg syncconf`: makes the running interface match `stripped` without
    /// dropping the tunnel.
    fn syncconf(&self, interface: &str, stripped: &str) -> Result<()>;
    /// Brings the interface up now and at boot.
    fn up(&self, interface: &str) -> Result<()>;
    /// Takes the interface down and keeps it from coming up at boot.
    fn down(&self, interface: &str) -> Result<()>;
    /// Rebuilds a running interface (addresses, routes, hooks changed).
    fn restart(&self, interface: &str) -> Result<()>;
    /// Whether the interface comes up at boot.
    fn enabled(&self, interface: &str) -> bool;
    /// The node's primary IPv4 address, a hint for the public endpoint.
    fn primary_address(&self) -> String;
}

pub struct SystemWg;

fn has_systemd() -> bool {
    has_command("systemctl") && Path::new("/run/systemd/system").exists()
}

fn unit(interface: &str) -> String {
    format!("wg-quick@{interface}")
}

impl SystemWg {
    fn ok(&self, cmd: &str, args: &[&str]) -> Result<String> {
        let (ok, out) = run_captured(cmd, args)?;
        if !ok {
            bail!("{cmd} {} failed: {}", args.join(" "), out.trim());
        }
        Ok(out)
    }
}

impl Wg for SystemWg {
    fn installed(&self) -> bool {
        has_command("wg") && has_command("wg-quick")
    }

    fn version(&self) -> String {
        run_captured("wg", &["--version"])
            .ok()
            .filter(|(ok, _)| *ok)
            .map(|(_, out)| parse_version(&out))
            .unwrap_or_default()
    }

    fn genkey(&self) -> Result<String> {
        Ok(self.ok("wg", &["genkey"])?.trim().to_string())
    }

    fn pubkey(&self, private_key: &str) -> Result<String> {
        let (ok, out) = run_with_input("wg", &["pubkey"], &format!("{private_key}\n"))?;
        if !ok {
            bail!("wg pubkey failed: {}", out.trim());
        }
        Ok(out.trim().to_string())
    }

    fn genpsk(&self) -> Result<String> {
        Ok(self.ok("wg", &["genpsk"])?.trim().to_string())
    }

    fn running(&self) -> Result<Vec<String>> {
        Ok(self
            .ok("wg", &["show", "interfaces"])?
            .split_whitespace()
            .map(str::to_string)
            .collect())
    }

    fn live_peers(&self, interface: &str) -> Result<Vec<LivePeer>> {
        parse_dump(&self.ok("wg", &["show", interface, "dump"])?)
    }

    fn strip(&self, conf: &Path) -> Result<String> {
        let path = conf.to_string_lossy();
        self.ok("wg-quick", &["strip", &path])
    }

    fn syncconf(&self, interface: &str, stripped: &str) -> Result<()> {
        let (ok, out) = run_with_input("wg", &["syncconf", interface, "/dev/stdin"], stripped)?;
        if !ok {
            bail!("wg syncconf {interface} failed: {}", out.trim());
        }
        Ok(())
    }

    fn up(&self, interface: &str) -> Result<()> {
        if has_systemd() {
            let unit = unit(interface);
            self.ok("systemctl", &["enable", &unit])?;
            // An interface brought up by hand already exists; the unit would
            // fail trying to create it again.
            if !self.running()?.iter().any(|name| name == interface)
                && let Err(err) = self.ok("systemctl", &["start", &unit])
            {
                // Do not leave a unit that cannot start enabled at boot, and say
                // why it failed: systemctl itself only points at the journal.
                let _ = run_captured("systemctl", &["disable", &unit]);
                let why = run_captured(
                    "journalctl",
                    &["-u", &unit, "-n", "6", "-o", "cat", "--no-pager"],
                )
                .map(|(_, out)| out.trim().to_string())
                .unwrap_or_default();
                if why.is_empty() {
                    return Err(err);
                }
                bail!("wg-quick could not bring {interface} up: {why}");
            }
            Ok(())
        } else {
            self.ok("wg-quick", &["up", interface]).map(|_| ())
        }
    }

    fn down(&self, interface: &str) -> Result<()> {
        if has_systemd() {
            let unit = unit(interface);
            let _ = run_captured("systemctl", &["disable", "--now", &unit]);
        }
        // Brought up by hand, or without systemd: the unit knows nothing of it.
        if self.running()?.iter().any(|name| name == interface) {
            self.ok("wg-quick", &["down", interface])?;
        }
        Ok(())
    }

    fn restart(&self, interface: &str) -> Result<()> {
        if has_systemd() {
            let unit = unit(interface);
            if run_captured("systemctl", &["restart", &unit]).is_ok_and(|(ok, _)| ok) {
                return Ok(());
            }
            // The unit was not what brought it up; do it by hand.
        }
        if self.running()?.iter().any(|name| name == interface) {
            self.ok("wg-quick", &["down", interface])?;
        }
        self.ok("wg-quick", &["up", interface]).map(|_| ())
    }

    fn enabled(&self, interface: &str) -> bool {
        has_systemd()
            && run_captured("systemctl", &["is-enabled", &unit(interface)])
                .is_ok_and(|(_, out)| out.trim() == "enabled")
    }

    fn primary_address(&self) -> String {
        run_captured("ip", &["-4", "route", "get", "1.1.1.1"])
            .ok()
            .filter(|(ok, _)| *ok)
            .map(|(_, out)| parse_route_src(&out))
            .unwrap_or_default()
    }
}

/// `wireguard-tools v1.0.20210914 - https://…` → `1.0.20210914`.
pub fn parse_version(out: &str) -> String {
    out.split_whitespace()
        .find_map(|word| {
            word.strip_prefix('v')
                .filter(|v| v.starts_with(|c: char| c.is_ascii_digit()))
        })
        .unwrap_or_default()
        .to_string()
}

/// `1.1.1.1 via 10.0.0.1 dev eth0 src 10.0.0.5 uid 0` → `10.0.0.5`.
pub fn parse_route_src(out: &str) -> String {
    let mut words = out.split_whitespace();
    while let Some(word) = words.next() {
        if word == "src" {
            return words.next().unwrap_or_default().to_string();
        }
    }
    String::new()
}

/// `wg show <iface> dump`: one tab-separated line for the interface, then
/// one per peer — public key, pre-shared key, endpoint, allowed IPs, latest
/// handshake, bytes received, bytes sent, keepalive.
pub fn parse_dump(out: &str) -> Result<Vec<LivePeer>> {
    let mut lines = out.lines().filter(|l| !l.trim().is_empty());
    if lines.next().is_none() {
        bail!("wg printed nothing for the interface");
    }
    let mut peers = Vec::new();
    for line in lines {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 8 {
            bail!("unexpected `wg show dump` line: {line}");
        }
        let number = |text: &str| -> Result<u64> {
            text.trim()
                .parse()
                .map_err(|_| anyhow::anyhow!("unexpected number '{text}' in `wg show dump`"))
        };
        peers.push(LivePeer {
            public_key: fields[0].to_string(),
            endpoint: if fields[2] == "(none)" {
                String::new()
            } else {
                fields[2].to_string()
            },
            latest_handshake_unix: i64::try_from(number(fields[4])?).unwrap_or(0),
            rx_bytes: number(fields[5])?,
            tx_bytes: number(fields[6])?,
        });
    }
    Ok(peers)
}

#[cfg(test)]
pub mod fake {
    //! A stand-in for the `wg` tools: keys are made-up but well-formed, and
    //! every call that changes something is recorded.

    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;
    use std::sync::Mutex;

    use anyhow::{Result, bail};

    use super::{LivePeer, Wg};

    #[derive(Default)]
    pub struct FakeState {
        pub running: BTreeSet<String>,
        pub enabled: BTreeSet<String>,
        pub live: BTreeMap<String, Vec<LivePeer>>,
        pub calls: Vec<String>,
        pub counter: u32,
        /// Makes the named call fail: `strip`, `syncconf`, `up` or `restart`.
        pub fail: BTreeSet<&'static str>,
        /// What `strip` last saw, to check what was applied.
        pub last_stripped: String,
    }

    #[derive(Default)]
    pub struct FakeWg {
        pub missing: bool,
        pub state: Mutex<FakeState>,
    }

    impl FakeWg {
        pub fn calls(&self) -> Vec<String> {
            self.state.lock().unwrap().calls.clone()
        }

        pub fn clear_calls(&self) {
            self.state.lock().unwrap().calls.clear();
        }

        pub fn set_running(&self, name: &str) {
            self.state.lock().unwrap().running.insert(name.to_string());
        }

        pub fn fail(&self, what: &'static str) {
            self.state.lock().unwrap().fail.insert(what);
        }

        pub fn heal(&self) {
            self.state.lock().unwrap().fail.clear();
        }

        pub fn set_live(&self, name: &str, peers: Vec<LivePeer>) {
            self.state
                .lock()
                .unwrap()
                .live
                .insert(name.to_string(), peers);
        }

        fn record(&self, call: String) -> std::sync::MutexGuard<'_, FakeState> {
            let mut state = self.state.lock().unwrap();
            state.calls.push(call);
            state
        }
    }

    fn key(prefix: char, n: u32) -> String {
        format!("{prefix}{n:042}=")
    }

    impl Wg for std::sync::Arc<FakeWg> {
        fn installed(&self) -> bool {
            !self.missing
        }
        fn version(&self) -> String {
            "1.0.20210914".to_string()
        }
        fn genkey(&self) -> Result<String> {
            let mut state = self.state.lock().unwrap();
            state.counter += 1;
            Ok(key('K', state.counter))
        }
        fn pubkey(&self, private_key: &str) -> Result<String> {
            // K… → P…: a stable, reversible stand-in for the curve.
            Ok(format!("P{}", &private_key[1..]))
        }
        fn genpsk(&self) -> Result<String> {
            let mut state = self.state.lock().unwrap();
            state.counter += 1;
            Ok(key('S', state.counter))
        }
        fn running(&self) -> Result<Vec<String>> {
            Ok(self.state.lock().unwrap().running.iter().cloned().collect())
        }
        fn live_peers(&self, interface: &str) -> Result<Vec<LivePeer>> {
            Ok(self
                .state
                .lock()
                .unwrap()
                .live
                .get(interface)
                .cloned()
                .unwrap_or_default())
        }
        fn strip(&self, conf: &Path) -> Result<String> {
            let mut state = self.record(format!(
                "strip {}",
                conf.file_name().unwrap().to_string_lossy()
            ));
            if state.fail.contains("strip") {
                bail!("wg-quick: syntax error");
            }
            let text = std::fs::read_to_string(conf)?;
            state.last_stripped = text.clone();
            Ok(text)
        }
        fn syncconf(&self, interface: &str, _stripped: &str) -> Result<()> {
            let state = self.record(format!("syncconf {interface}"));
            if state.fail.contains("syncconf") {
                bail!("wg syncconf failed");
            }
            Ok(())
        }
        fn up(&self, interface: &str) -> Result<()> {
            let mut state = self.record(format!("up {interface}"));
            if state.fail.contains("up") {
                bail!("wg-quick up failed");
            }
            state.running.insert(interface.to_string());
            state.enabled.insert(interface.to_string());
            Ok(())
        }
        fn down(&self, interface: &str) -> Result<()> {
            let mut state = self.record(format!("down {interface}"));
            state.running.remove(interface);
            state.enabled.remove(interface);
            Ok(())
        }
        fn restart(&self, interface: &str) -> Result<()> {
            let mut state = self.record(format!("restart {interface}"));
            if state.fail.contains("restart") {
                bail!("restart failed");
            }
            state.running.insert(interface.to_string());
            Ok(())
        }
        fn enabled(&self, interface: &str) -> bool {
            self.state.lock().unwrap().enabled.contains(interface)
        }
        fn primary_address(&self) -> String {
            "198.51.100.7".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_is_read_from_the_banner() {
        assert_eq!(
            parse_version(
                "wireguard-tools v1.0.20210914 - https://git.zx2c4.com/wireguard-tools/\n"
            ),
            "1.0.20210914"
        );
        assert_eq!(parse_version("nothing here"), "");
    }

    #[test]
    fn the_source_address_comes_from_the_route() {
        assert_eq!(
            parse_route_src("1.1.1.1 via 10.0.0.1 dev eth0 src 10.0.0.5 uid 0\n    cache\n"),
            "10.0.0.5"
        );
        assert_eq!(
            parse_route_src("RTNETLINK answers: Network is unreachable"),
            ""
        );
    }

    #[test]
    fn the_dump_gives_each_peers_handshake_and_traffic() {
        let dump = "PRIV\tPUB\t51820\toff\n\
            PEER1\tPSK\t203.0.113.9:40000\t10.8.0.2/32\t1700000000\t1024\t2048\t25\n\
            PEER2\t(none)\t(none)\t10.8.0.3/32\t0\t0\t0\toff\n";
        let peers = parse_dump(dump).unwrap();
        assert_eq!(peers.len(), 2);
        assert_eq!(
            peers[0],
            LivePeer {
                public_key: "PEER1".into(),
                endpoint: "203.0.113.9:40000".into(),
                latest_handshake_unix: 1_700_000_000,
                rx_bytes: 1024,
                tx_bytes: 2048,
            }
        );
        assert_eq!(peers[1].endpoint, "");
        assert_eq!(peers[1].latest_handshake_unix, 0);
    }

    #[test]
    fn a_dump_of_an_unknown_shape_is_an_error_not_an_empty_list() {
        assert!(parse_dump("").is_err());
        assert!(parse_dump("PRIV\tPUB\t1\toff\nPEER\tonly\tthree\n").is_err());
        assert!(parse_dump("PRIV\tPUB\t1\toff\nP\tK\t(none)\tx\tabc\t0\t0\toff\n").is_err());
    }
}
