//! Real host listening-port inventory (DMN-103): what is actually bound on
//! this machine, parsed straight from `/proc/net/{tcp,tcp6,udp,udp6}` — not
//! to be confused with [`crate::daemon::apps::ports::published`], which
//! reports what an app's settings merely *declare*. Neither list is the
//! other: an installed app that is currently stopped has nothing bound here
//! at all, and a hand-run process ASC knows nothing about shows up here but
//! never there.
//!
//! Attribution to a Docker container cannot be done from `/proc` alone:
//! with the Engine's default userland proxy the listener is `docker-proxy`,
//! with `--userland-proxy=false` it is `dockerd` itself — neither name says
//! which container owns the port. That cross-reference is the caller's job
//! (see `ApiState::listening_ports`), matched against
//! [`crate::daemon::docker::list_containers`]'s published ports.

use std::collections::HashMap;
use std::fs;
use std::net::{Ipv4Addr, Ipv6Addr};

use serde::{Deserialize, Serialize};

/// One socket found in `/proc/net/{tcp,tcp6,udp,udp6}`, before any
/// cross-referencing against Docker or the app store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListeningSocket {
    pub port: u16,
    /// "tcp" or "udp".
    pub protocol: &'static str,
    /// Bind address exactly as `/proc` reports it ("0.0.0.0", "::",
    /// "127.0.0.1") — not normalized or deduplicated across families.
    pub address: String,
    /// "ipv4" or "ipv6".
    pub family: &'static str,
    pub pid: Option<u32>,
    /// `/proc/<pid>/comm`, absent when `pid` is absent.
    pub process: Option<String>,
    /// `/proc/<pid>/cmdline`, argv joined with spaces; absent when `pid` is
    /// absent or the process has already exited by the time it is read.
    pub command: Option<String>,
}

/// A row of `/proc/net/{tcp,tcp6,udp,udp6}` after decoding the hex fields,
/// before the TCP-vs-UDP filtering rule is applied.
struct ProcNetRow {
    address: String,
    port: u16,
    /// True when the remote address:port is all-zero — the shape a UDP
    /// socket bound with `bind()` (not `connect()`ed anywhere) has.
    remote_is_zero: bool,
    /// Two-hex-digit connection state ("0A" = TCP_LISTEN); UDP sockets
    /// always report "07" (TCP_CLOSE's numeric value, reused because the
    /// kernel has no UDP-specific state machine) and the state is ignored
    /// for them.
    state: String,
    inode: u64,
}

/// Decode one `/proc/net/tcp`-shaped table. `family` is stamped onto every
/// row rather than sniffed from the content, since the file itself never
/// says which address family it holds — that is purely which path was read.
fn parse_proc_net_rows(raw: &str, family: &'static str) -> Vec<ProcNetRow> {
    let mut rows = Vec::new();
    for line in raw.lines().skip(1) {
        let mut fields = line.split_whitespace();
        // sl local_address rem_address st tx_queue:rx_queue tr:tm->when
        // retrnsmt uid timeout inode ...
        //
        // The leading `sl` column ("0:", "1:", ...) must be consumed and
        // discarded before `local_address` — skipping it was the one bug
        // this parser cannot afford: get it wrong and every row's fields
        // shift by one, so `local` ends up holding "0:" and every port
        // fails to parse, silently returning zero listening ports instead
        // of erroring.
        let Some(_sl) = fields.next() else { continue };
        let Some(local) = fields.next() else { continue };
        let Some(remote) = fields.next() else {
            continue;
        };
        let Some(state) = fields.next() else { continue };
        // Position is now just past `st` (index 3). `inode` is index 9, so
        // skip tx_queue:rx_queue, tr:tm->when, retrnsmt, uid, timeout (5
        // fields, indices 4-8) and take the next one.
        let Some(inode_field) = fields.nth(5) else {
            continue;
        };
        let Some((addr_hex, port_hex)) = local.split_once(':') else {
            continue;
        };
        let Some(port) = u16::from_str_radix(port_hex, 16).ok() else {
            continue;
        };
        let Some(address) = decode_address_hex(addr_hex, family) else {
            continue;
        };
        let remote_is_zero = remote
            .split_once(':')
            .map(|(remote_addr, remote_port)| {
                remote_addr.chars().all(|c| c == '0') && remote_port.chars().all(|c| c == '0')
            })
            .unwrap_or(false);
        let Ok(inode) = inode_field.parse::<u64>() else {
            continue;
        };
        rows.push(ProcNetRow {
            address,
            port,
            remote_is_zero,
            state: state.to_ascii_uppercase(),
            inode,
        });
    }
    rows
}

/// `TCP_LISTEN` in the kernel's `enum` — the only TCP state this module
/// treats as "listening".
const TCP_LISTEN: &str = "0A";

/// Decode the hex `local_address` field of one `/proc/net/*` row into a
/// display string. IPv4 is a single 32-bit little-endian word; IPv6 is four
/// such words concatenated, each byte-swapped independently — the classic
/// mistake is treating the 32 hex chars as one big-endian blob, which
/// produces a byte-reversed, wrong address instead of a parse error, so it
/// is easy to ship silently broken.
fn decode_address_hex(hex: &str, family: &'static str) -> Option<String> {
    match family {
        "ipv4" => {
            if hex.len() != 8 {
                return None;
            }
            let word = u32::from_str_radix(hex, 16).ok()?;
            Some(Ipv4Addr::from(word.to_le_bytes()).to_string())
        }
        "ipv6" => {
            if hex.len() != 32 {
                return None;
            }
            let mut bytes = [0u8; 16];
            for i in 0..4 {
                let word_hex = &hex[i * 8..i * 8 + 8];
                let word = u32::from_str_radix(word_hex, 16).ok()?;
                bytes[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
            }
            Some(Ipv6Addr::from(bytes).to_string())
        }
        _ => None,
    }
}

/// Every TCP socket in `TCP_LISTEN`, across both address families passed.
fn tcp_listening(raw_v4: &str, raw_v6: &str) -> Vec<ProcNetRow> {
    let mut rows = parse_proc_net_rows(raw_v4, "ipv4");
    rows.extend(parse_proc_net_rows(raw_v6, "ipv6"));
    rows.retain(|row| row.state == TCP_LISTEN);
    rows
}

/// Every UDP socket bound (not connected) to a local address — UDP has no
/// `LISTEN` state, so "bound" is the closest equivalent: a socket that
/// never called `connect()` reports an all-zero remote address:port.
fn udp_bound(raw_v4: &str, raw_v6: &str) -> Vec<ProcNetRow> {
    let mut rows = parse_proc_net_rows(raw_v4, "ipv4");
    rows.extend(parse_proc_net_rows(raw_v6, "ipv6"));
    rows.retain(|row| row.remote_is_zero);
    rows
}

/// Cap on the number of `/proc/<pid>/fd/*` entries this module will ever
/// examine in one call. A pathological host (tens of thousands of open
/// files) must not turn "open the Ports tab" into a multi-second stall;
/// missing an attribution past this point degrades to an unowned row, not
/// an error.
const MAX_FD_ENTRIES: usize = 50_000;

/// `socket:[12345]` -> `12345`, the shape `/proc/<pid>/fd/<n>` symlinks take
/// for an open socket. Any other target (a regular file, a pipe, `/dev/null`)
/// returns `None`.
fn parse_socket_inode(link_target: &str) -> Option<u64> {
    link_target
        .strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

/// Map socket inode -> (pid, `comm`) for every process this caller can see
/// into. Bounded and best-effort: `EACCES`/`ENOENT` on any single entry is a
/// skip, not a failure — a process can exit between `readdir` and
/// `readlink` at any point, and a non-root caller cannot read most other
/// users' `/proc/<pid>/fd` at all, which is not a bug to report, just an
/// empty contribution to the map.
fn inode_owners() -> HashMap<u64, (u32, String)> {
    let mut map = HashMap::new();
    let mut scanned = 0usize;
    let Ok(proc_entries) = fs::read_dir("/proc") else {
        return map;
    };
    for entry in proc_entries.flatten() {
        if scanned >= MAX_FD_ENTRIES {
            break;
        }
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(fds) = fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        let comm = fs::read_to_string(entry.path().join("comm"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        for fd in fds.flatten() {
            scanned += 1;
            if scanned > MAX_FD_ENTRIES {
                break;
            }
            let Ok(target) = fs::read_link(fd.path()) else {
                continue;
            };
            let Some(inode) = target.to_str().and_then(parse_socket_inode) else {
                continue;
            };
            // The first pid seen for a shared listening socket (SO_REUSEPORT,
            // or a fork before an accept loop) wins; any one owner is a
            // reasonable answer for a UI, and there is no "the" owner.
            map.entry(inode).or_insert((pid, comm.clone()));
        }
    }
    map
}

/// `/proc/<pid>/cmdline`'s NUL-separated argv, joined with spaces. `None`
/// when the process has already exited or the file cannot be read — a race
/// with the same shape as the fd walk above.
fn read_cmdline(pid: u32) -> Option<String> {
    let raw = fs::read_to_string(format!("/proc/{pid}/cmdline")).ok()?;
    let joined = raw
        .split('\0')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    (!joined.is_empty()).then_some(joined)
}

/// Every TCP socket in `LISTEN` and every UDP socket bound to a local
/// address, with process attribution filled in where the caller has
/// permission to see it. Blocking: reads `/proc/net/*` and walks
/// `/proc/*/fd`; call from a blocking task, same convention as
/// [`super::network::list_interfaces`].
pub fn listening() -> Vec<ListeningSocket> {
    let read = |path: &str| fs::read_to_string(path).unwrap_or_default();
    let tcp_rows = tcp_listening(&read("/proc/net/tcp"), &read("/proc/net/tcp6"));
    let udp_rows = udp_bound(&read("/proc/net/udp"), &read("/proc/net/udp6"));

    let owners = inode_owners();
    // A pid's cmdline is read at most once per call even if it owns several
    // listening sockets (a server with more than one bound port is common).
    let mut cmdlines: HashMap<u32, Option<String>> = HashMap::new();

    let mut to_socket = |row: ProcNetRow, protocol: &'static str, family: &'static str| {
        let owner = owners.get(&row.inode).cloned();
        let (pid, process) = match owner {
            Some((pid, comm)) => (Some(pid), Some(comm)),
            None => (None, None),
        };
        let command = pid.and_then(|pid| {
            cmdlines
                .entry(pid)
                .or_insert_with(|| read_cmdline(pid))
                .clone()
        });
        ListeningSocket {
            port: row.port,
            protocol,
            address: row.address,
            family,
            pid,
            process,
            command,
        }
    };

    let mut out: Vec<ListeningSocket> = Vec::with_capacity(tcp_rows.len() + udp_rows.len());
    for row in tcp_rows {
        let family = row_family(&row);
        out.push(to_socket(row, "tcp", family));
    }
    for row in udp_rows {
        let family = row_family(&row);
        out.push(to_socket(row, "udp", family));
    }
    out
}

/// The family a row was parsed under is not stored on `ProcNetRow` itself
/// (it drives *how* the row was decoded, not a field of it), so recover it
/// from the address string's own shape — an IPv4 dotted-quad never contains
/// a colon.
fn row_family(row: &ProcNetRow) -> &'static str {
    if row.address.contains(':') {
        "ipv6"
    } else {
        "ipv4"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixture lines mirror the real /proc/net/tcp header + row shape:
    // sl  local_address rem_address   st tx_rx:tr_tm->when retrnsmt uid timeout inode

    #[test]
    fn decodes_ipv4_loopback_and_port() {
        let addr = decode_address_hex("0100007F", "ipv4").unwrap();
        assert_eq!(addr, "127.0.0.1");
    }

    #[test]
    fn decodes_ipv4_any_address() {
        let addr = decode_address_hex("00000000", "ipv4").unwrap();
        assert_eq!(addr, "0.0.0.0");
    }

    /// Builds the /proc/net/tcp6-shaped hex encoding of an IPv6 address: four
    /// 32-bit words, each word's bytes reversed independently — the inverse
    /// of what decode_address_hex undoes. Constructing fixtures this way
    /// (rather than a hand-typed 32-char literal) is itself a check that the
    /// encode/decode pair agree on the word-swap direction.
    fn encode_ipv6_hex(ip: Ipv6Addr) -> String {
        let bytes = ip.octets();
        let mut hex = String::with_capacity(32);
        for word in bytes.as_chunks::<4>().0 {
            let mut word_le = [0u8; 4];
            word_le.copy_from_slice(word);
            word_le.reverse();
            hex.push_str(&format!("{:08X}", u32::from_be_bytes(word_le)));
        }
        hex
    }

    #[test]
    fn decodes_ipv6_loopback() {
        let hex = encode_ipv6_hex(Ipv6Addr::LOCALHOST);
        assert_eq!(hex.len(), 32);
        let addr = decode_address_hex(&hex, "ipv6").unwrap();
        assert_eq!(addr, "::1");
    }

    #[test]
    fn decodes_ipv6_round_trips_a_non_trivial_address() {
        let ip: Ipv6Addr = "2001:db8::ff00:42:8329".parse().unwrap();
        let hex = encode_ipv6_hex(ip);
        let addr = decode_address_hex(&hex, "ipv6").unwrap();
        assert_eq!(addr, ip.to_string());
    }

    #[test]
    fn decodes_ipv6_any_address() {
        let addr = decode_address_hex("00000000000000000000000000000000", "ipv6").unwrap();
        assert_eq!(addr, "::");
    }

    #[test]
    fn tcp_listening_keeps_only_listen_state() {
        let header = "  sl  local_address rem_address   st tx_queue:rx_queue tr:tm->when retrnsmt   uid  timeout inode";
        let listening = format!(
            "{header}\n   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 12345 1 0000000000000000 100 0 0 10 0"
        );
        let established = "   1: 0100007F:1F90 0100007F:C350 01 00000000:00000000 00:00000000 00000000     0        0 12346 1 0000000000000000 100 0 0 10 0";
        let raw = format!("{listening}\n{established}\n");
        let rows = tcp_listening(&raw, "");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].port, 0x1F90);
        assert_eq!(rows[0].address, "127.0.0.1");
        assert_eq!(rows[0].inode, 12345);
    }

    #[test]
    fn udp_bound_keeps_only_zero_remote() {
        let header = "  sl  local_address rem_address   st tx_queue:rx_queue tr:tm->when retrnsmt   uid  timeout inode";
        let bound = "   0: 00000000:0035 00000000:0000 07 00000000:00000000 00:00000000 00000000     0        0 22222 2 0000000000000000 0";
        let connected = "   1: 0100007F:C350 0100007F:0035 07 00000000:00000000 00:00000000 00000000     0        0 22223 2 0000000000000000 0";
        let raw = format!("{header}\n{bound}\n{connected}\n");
        let rows = udp_bound(&raw, "");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].port, 0x35);
        assert_eq!(rows[0].inode, 22222);
    }

    #[test]
    fn parse_socket_inode_matches_the_engine_link_shape() {
        assert_eq!(parse_socket_inode("socket:[12345]"), Some(12345));
        assert_eq!(parse_socket_inode("/dev/null"), None);
        assert_eq!(parse_socket_inode("pipe:[999]"), None);
    }

    #[test]
    fn a_malformed_row_is_skipped_not_fatal() {
        let header = "  sl  local_address rem_address   st tx_queue:rx_queue tr:tm->when retrnsmt   uid  timeout inode";
        let malformed = "   0: not-hex:zz 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1 1 0 0 0 0 0";
        let raw = format!("{header}\n{malformed}\n");
        let rows = tcp_listening(&raw, "ipv4");
        assert!(rows.is_empty());
    }
}
