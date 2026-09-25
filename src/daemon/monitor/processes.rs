//! Host process inventory and signals (DMN-119): what `ps`/`top` would show,
//! parsed straight from `/proc/<pid>/{stat,status,cmdline,cgroup}`, plus the
//! one control a process manager needs — delivering a signal.
//!
//! CPU usage is a delta of two readings, like everywhere else in this
//! module tree. Instead of sleeping on every call, the previous reading is
//! kept in a process-wide slot: a UI polling the list every few seconds gets
//! the share since its own previous poll for free, and only a cold call (or
//! one after a long pause) pays a short sampling interval.
//!
//! Identity is `(pid, start_ticks)`, not the pid alone: pids are recycled,
//! and a signal aimed at "the nginx row the operator was looking at" must
//! not land on whatever unrelated process inherited that number since the
//! list was rendered. [`signal`] takes the start time the caller saw and
//! refuses when it no longer matches.

use std::collections::HashMap;
use std::fs;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::system::{CpuTimes, parse_core_count, parse_cpu_times};

/// `PF_KTHREAD` from `include/linux/sched.h`: the `flags` field of
/// `/proc/<pid>/stat` has it set for kernel threads, which have no userspace
/// to signal and no cmdline.
const PF_KTHREAD: u64 = 0x0020_0000;

/// A cmdline longer than this is cut: a process list is for recognizing a
/// process, and a Java classpath can run into hundreds of kilobytes.
const MAX_COMMAND_CHARS: usize = 4096;

/// Sampling interval for a cold call (no usable previous reading).
const COLD_SAMPLE: Duration = Duration::from_millis(300);

/// A previous reading older than this is discarded rather than used: a
/// share averaged over the last ten minutes is not what a live list shows.
const MAX_REUSE_AGE: Duration = Duration::from_secs(60);

/// A previous reading younger than this is too short a window for a stable
/// percentage (two polls racing each other); sample afresh instead.
const MIN_REUSE_AGE: Duration = Duration::from_millis(200);

/// One process as the API reports it.
#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    /// `/proc/<pid>/comm` as it appears in `stat` — at most 15 bytes.
    pub name: String,
    /// argv joined with spaces; empty for kernel threads and zombies.
    pub command: String,
    /// Effective uid.
    pub uid: u32,
    /// Account name for `uid`, or the number when it has no passwd entry.
    pub user: String,
    /// Single-letter kernel state: R, S, D, Z, T, t, I, X.
    pub state: String,
    /// CPU share since the previous reading, `top`-style: 100 = one full
    /// core, so a multi-threaded process can exceed 100.
    pub cpu_percent: f64,
    pub rss_bytes: u64,
    pub virtual_bytes: u64,
    pub threads: u32,
    pub nice: i32,
    /// Unix seconds; 0 when the boot time could not be read.
    pub started_at: i64,
    /// Raw `starttime` (clock ticks since boot) — the identity token
    /// [`signal`] checks against pid reuse.
    pub start_ticks: u64,
    pub kernel_thread: bool,
    /// Full 64-hex Docker container id from `/proc/<pid>/cgroup`, when the
    /// process runs inside one.
    pub container_id: Option<String>,
    /// Why [`signal`] refuses this process, if it does.
    pub protected_reason: Option<&'static str>,
}

/// The inventory plus the host totals a UI needs to render shares.
#[derive(Debug, Clone)]
pub struct ProcessList {
    pub processes: Vec<ProcessInfo>,
    pub memory_total_bytes: u64,
    pub cpu_count: u32,
}

/// Signals an operator may send. Deliberately a closed set rather than a
/// raw number: the numbering differs between architectures, and the rest
/// (SIGSEGV, SIGABRT…) are not something a process manager hands out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Term,
    Kill,
    Hup,
    Int,
    Stop,
    Cont,
    Usr1,
    Usr2,
}

impl Signal {
    /// REST spelling: `term`, `kill`, `hup`, `int`, `stop`, `cont`, `usr1`,
    /// `usr2` (the `SIG` prefix is accepted too, case-insensitively).
    pub fn from_wire(value: &str) -> Option<Self> {
        let lower = value.trim().to_ascii_lowercase();
        let name = lower.strip_prefix("sig").unwrap_or(&lower);
        match name {
            "term" => Some(Self::Term),
            "kill" => Some(Self::Kill),
            "hup" => Some(Self::Hup),
            "int" => Some(Self::Int),
            "stop" => Some(Self::Stop),
            "cont" => Some(Self::Cont),
            "usr1" => Some(Self::Usr1),
            "usr2" => Some(Self::Usr2),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Term => "SIGTERM",
            Self::Kill => "SIGKILL",
            Self::Hup => "SIGHUP",
            Self::Int => "SIGINT",
            Self::Stop => "SIGSTOP",
            Self::Cont => "SIGCONT",
            Self::Usr1 => "SIGUSR1",
            Self::Usr2 => "SIGUSR2",
        }
    }

    fn raw(self) -> libc::c_int {
        match self {
            Self::Term => libc::SIGTERM,
            Self::Kill => libc::SIGKILL,
            Self::Hup => libc::SIGHUP,
            Self::Int => libc::SIGINT,
            Self::Stop => libc::SIGSTOP,
            Self::Cont => libc::SIGCONT,
            Self::Usr1 => libc::SIGUSR1,
            Self::Usr2 => libc::SIGUSR2,
        }
    }
}

/// Why [`signal`] did not deliver. Typed so the transports can map each to
/// its own status code instead of matching on message text.
#[derive(Debug)]
pub enum ProcessError {
    /// No such pid (or it exited before the signal went out).
    NotFound(u32),
    /// The pid exists but was reused: its start time differs from the one
    /// the caller saw.
    Replaced(u32),
    /// pid 1, this daemon, or a kernel thread.
    Protected {
        pid: u32,
        reason: &'static str,
    },
    /// pid 0 or out of `pid_t` range.
    InvalidPid(u32),
    /// The kernel refused (EPERM) — only possible for a non-root daemon.
    PermissionDenied(u32),
    Io(u32, std::io::Error),
}

impl std::fmt::Display for ProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(pid) => write!(f, "process {pid} not found"),
            Self::Replaced(pid) => write!(
                f,
                "process {pid} has exited and its pid was reused by another process"
            ),
            Self::Protected { pid, reason } => {
                write!(f, "process {pid} is protected: {reason}")
            }
            Self::InvalidPid(pid) => write!(f, "invalid pid {pid}"),
            Self::PermissionDenied(pid) => {
                write!(f, "not permitted to signal process {pid}")
            }
            Self::Io(pid, err) => write!(f, "cannot signal process {pid}: {err}"),
        }
    }
}

impl std::error::Error for ProcessError {}

/// Fields of `/proc/<pid>/stat` this module uses.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stat {
    name: String,
    state: char,
    ppid: u32,
    flags: u64,
    /// utime + stime, clock ticks.
    cpu_ticks: u64,
    nice: i32,
    threads: u32,
    start_ticks: u64,
    virtual_bytes: u64,
    rss_pages: u64,
}

/// Parse one `/proc/<pid>/stat` line. `comm` sits in parentheses and may
/// itself contain spaces and `)` — everything up to the *last* `)` is the
/// name, and the fixed-position fields follow it.
fn parse_stat(raw: &str) -> Option<Stat> {
    let open = raw.find('(')?;
    let close = raw.rfind(')')?;
    if close < open {
        return None;
    }
    let name = raw[open + 1..close].to_string();
    // Fields from `state` (field 3 in proc(5)) onward.
    let rest: Vec<&str> = raw[close + 1..].split_whitespace().collect();
    // proc(5) numbers fields from 1; `state` is 3, so field N is rest[N - 3].
    let field = |n: usize| rest.get(n - 3).copied();
    let num = |n: usize| field(n).and_then(|v| v.parse::<u64>().ok());
    let state = field(3)?.chars().next()?;
    Some(Stat {
        name,
        state,
        ppid: num(4)? as u32,
        flags: num(9)?,
        cpu_ticks: num(14)?.saturating_add(num(15)?),
        nice: field(19)?.parse().ok()?,
        threads: num(20)? as u32,
        start_ticks: num(22)?,
        virtual_bytes: num(23)?,
        rss_pages: num(24)?,
    })
}

/// Effective uid from the `Uid:` line of `/proc/<pid>/status`
/// (real, effective, saved, fs).
fn parse_status_euid(raw: &str) -> Option<u32> {
    raw.lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// Docker container id from `/proc/<pid>/cgroup`. Covers both cgroup
/// drivers: cgroupfs (`/docker/<id>`) and systemd (`docker-<id>.scope`), on
/// cgroup v1 and v2 alike — the id is the only 64-hex token in either shape.
fn parse_cgroup_container(raw: &str) -> Option<String> {
    for line in raw.lines() {
        let path = line.rsplit(':').next().unwrap_or_default();
        for part in path.split('/') {
            let candidate = part
                .strip_prefix("docker-")
                .and_then(|p| p.strip_suffix(".scope"))
                .unwrap_or(part);
            if candidate.len() == 64
                && candidate.bytes().all(|b| b.is_ascii_hexdigit())
                && path.contains("docker")
            {
                return Some(candidate.to_string());
            }
        }
    }
    None
}

/// `/proc/<pid>/cmdline`'s NUL-separated argv joined with spaces, cut at
/// [`MAX_COMMAND_CHARS`].
fn read_command(pid: u32) -> String {
    let Ok(raw) = fs::read(format!("/proc/{pid}/cmdline")) else {
        return String::new();
    };
    let text = String::from_utf8_lossy(&raw);
    let mut joined = text
        .split('\0')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if joined.chars().count() > MAX_COMMAND_CHARS {
        joined = joined.chars().take(MAX_COMMAND_CHARS).collect();
        joined.push('…');
    }
    joined
}

/// uid -> account name, from `/etc/passwd` read once per call.
fn passwd_names() -> HashMap<u32, String> {
    let raw = fs::read_to_string("/etc/passwd").unwrap_or_default();
    raw.lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let mut fields = line.split(':');
            let name = fields.next()?;
            fields.next()?;
            let uid = fields.next()?.parse().ok()?;
            Some((uid, name.to_string()))
        })
        .collect()
}

/// `btime` of `/proc/stat`: boot time in unix seconds.
fn parse_boot_time(stat: &str) -> Option<i64> {
    stat.lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()
}

fn clock_ticks_per_second() -> u64 {
    // SAFETY: sysconf has no preconditions.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks > 0 { ticks as u64 } else { 100 }
}

fn page_size() -> u64 {
    // SAFETY: sysconf has no preconditions.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if size > 0 { size as u64 } else { 4096 }
}

/// Why a process may never be signalled through the API, if it may not.
fn protection(pid: u32, stat: &Stat) -> Option<&'static str> {
    if pid == 1 {
        Some("init (pid 1)")
    } else if pid == std::process::id() {
        Some("the asc daemon itself")
    } else if stat.flags & PF_KTHREAD != 0 || pid == 2 {
        Some("kernel thread")
    } else {
        None
    }
}

/// One full `/proc` walk: per-pid CPU ticks keyed by `(pid, start_ticks)`
/// plus the aggregate CPU counters taken at the same moment.
struct Reading {
    at: Instant,
    cpu: CpuTimes,
    ticks: HashMap<(u32, u64), u64>,
}

/// The previous reading, shared by every caller (see the module docs).
static PREVIOUS: Mutex<Option<Reading>> = Mutex::new(None);

struct Raw {
    pid: u32,
    stat: Stat,
}

fn walk() -> Vec<Raw> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            // A process can exit between readdir and read: a skip, not an error.
            let raw = fs::read_to_string(entry.path().join("stat")).ok()?;
            Some(Raw {
                pid,
                stat: parse_stat(&raw)?,
            })
        })
        .collect()
}

fn read_cpu() -> Option<(CpuTimes, u32, Option<i64>)> {
    let stat = fs::read_to_string("/proc/stat").ok()?;
    Some((
        parse_cpu_times(&stat)?,
        parse_core_count(&stat).max(1),
        parse_boot_time(&stat),
    ))
}

fn reading_of(raws: &[Raw], cpu: CpuTimes) -> Reading {
    Reading {
        at: Instant::now(),
        cpu,
        ticks: raws
            .iter()
            .map(|r| ((r.pid, r.stat.start_ticks), r.stat.cpu_ticks))
            .collect(),
    }
}

/// CPU share of one process between two readings, `top`-style (100 = one
/// core). `total_delta` is the aggregate over all cores, so it is scaled
/// back to one core by `cores`.
fn cpu_share(proc_delta: u64, total_delta: u64, cores: u32) -> f64 {
    if total_delta == 0 {
        return 0.0;
    }
    proc_delta as f64 / total_delta as f64 * cores as f64 * 100.0
}

/// Every process on the host. Kernel threads are left out unless
/// `include_kernel_threads` — there are hundreds of them on a large machine
/// and nothing to do with them. Blocking: walks `/proc`, and a cold call
/// sleeps [`COLD_SAMPLE`] for the CPU delta; call from a blocking task.
pub fn list(include_kernel_threads: bool) -> ProcessList {
    let (mut cpu_now, cores, boot) =
        read_cpu().unwrap_or((CpuTimes { busy: 0, total: 0 }, 1, None));
    let mut raws = walk();

    let mut previous = PREVIOUS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let usable = previous.as_ref().is_some_and(|prev| {
        let age = prev.at.elapsed();
        (MIN_REUSE_AGE..=MAX_REUSE_AGE).contains(&age)
    });
    if !usable {
        *previous = Some(reading_of(&raws, cpu_now));
        drop(previous);
        std::thread::sleep(COLD_SAMPLE);
        if let Some((cpu, _, _)) = read_cpu() {
            cpu_now = cpu;
        }
        raws = walk();
        previous = PREVIOUS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
    let baseline = previous.take();
    *previous = Some(reading_of(&raws, cpu_now));
    drop(previous);

    let total_delta = baseline
        .as_ref()
        .map(|b| cpu_now.total.saturating_sub(b.cpu.total))
        .unwrap_or(0);

    let names = passwd_names();
    let ticks_per_second = clock_ticks_per_second();
    let page = page_size();

    let processes = raws
        .into_iter()
        .filter(|r| include_kernel_threads || r.stat.flags & PF_KTHREAD == 0)
        .map(|Raw { pid, stat }| {
            let kernel_thread = stat.flags & PF_KTHREAD != 0;
            let uid = fs::read_to_string(format!("/proc/{pid}/status"))
                .ok()
                .and_then(|raw| parse_status_euid(&raw))
                .unwrap_or(0);
            let before = baseline
                .as_ref()
                .and_then(|b| b.ticks.get(&(pid, stat.start_ticks)).copied());
            // A process born after the baseline has no earlier reading: its
            // whole life fits inside the window, so all of its ticks count.
            let proc_delta = stat.cpu_ticks.saturating_sub(before.unwrap_or(0));
            let container_id = if kernel_thread {
                None
            } else {
                fs::read_to_string(format!("/proc/{pid}/cgroup"))
                    .ok()
                    .and_then(|raw| parse_cgroup_container(&raw))
            };
            ProcessInfo {
                pid,
                ppid: stat.ppid,
                command: if kernel_thread {
                    String::new()
                } else {
                    read_command(pid)
                },
                uid,
                user: names.get(&uid).cloned().unwrap_or_else(|| uid.to_string()),
                state: stat.state.to_string(),
                cpu_percent: cpu_share(proc_delta, total_delta, cores),
                rss_bytes: stat.rss_pages.saturating_mul(page),
                virtual_bytes: stat.virtual_bytes,
                threads: stat.threads,
                nice: stat.nice,
                started_at: boot
                    .map(|boot| boot + (stat.start_ticks / ticks_per_second) as i64)
                    .unwrap_or(0),
                start_ticks: stat.start_ticks,
                kernel_thread,
                container_id,
                protected_reason: protection(pid, &stat),
                name: stat.name,
            }
        })
        .collect();

    let memory_total_bytes = fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|raw| super::system::parse_meminfo(&raw))
        .map(|m| m.total)
        .unwrap_or(0);

    ProcessList {
        processes,
        memory_total_bytes,
        cpu_count: cores,
    }
}

/// Deliver `sig` to `pid`. `expected_start_ticks`, when given, must match
/// the process's current start time — otherwise the pid was reused since
/// the caller looked and the call is refused with [`ProcessError::Replaced`].
/// Protected processes (see [`protection`]) are refused before anything is
/// sent.
pub fn signal(
    pid: u32,
    sig: Signal,
    expected_start_ticks: Option<u64>,
) -> Result<(), ProcessError> {
    let Ok(raw_pid) = libc::pid_t::try_from(pid) else {
        return Err(ProcessError::InvalidPid(pid));
    };
    if raw_pid <= 0 {
        return Err(ProcessError::InvalidPid(pid));
    }
    let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(raw) => parse_stat(&raw).ok_or(ProcessError::NotFound(pid))?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(ProcessError::NotFound(pid));
        }
        Err(err) => return Err(ProcessError::Io(pid, err)),
    };
    if let Some(reason) = protection(pid, &stat) {
        return Err(ProcessError::Protected { pid, reason });
    }
    if expected_start_ticks.is_some_and(|expected| expected != stat.start_ticks) {
        return Err(ProcessError::Replaced(pid));
    }
    // SAFETY: kill(2) has no memory-safety preconditions; `raw_pid` is a
    // positive pid, never 0/-1, so it cannot address a process group or
    // "every process".
    if unsafe { libc::kill(raw_pid, sig.raw()) } == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    Err(match err.raw_os_error() {
        Some(libc::ESRCH) => ProcessError::NotFound(pid),
        Some(libc::EPERM) => ProcessError::PermissionDenied(pid),
        _ => ProcessError::Io(pid, err),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NGINX_STAT: &str = "1234 (nginx) S 1 1234 1234 0 -1 4194560 700 0 0 0 150 50 0 0 20 0 3 0 98765 123456789 2048 18446744073709551615 1 1 0 0 0 0 0 4096 0 0 0 0 17 0 0 0 0 0 0";

    #[test]
    fn stat_parses_fixed_fields() {
        let stat = parse_stat(NGINX_STAT).unwrap();
        assert_eq!(stat.name, "nginx");
        assert_eq!(stat.state, 'S');
        assert_eq!(stat.ppid, 1);
        assert_eq!(stat.cpu_ticks, 200);
        assert_eq!(stat.nice, 0);
        assert_eq!(stat.threads, 3);
        assert_eq!(stat.start_ticks, 98765);
        assert_eq!(stat.virtual_bytes, 123456789);
        assert_eq!(stat.rss_pages, 2048);
        assert_eq!(stat.flags & PF_KTHREAD, 0);
    }

    #[test]
    fn stat_name_may_contain_spaces_and_parens() {
        let raw = NGINX_STAT.replace("(nginx)", "(my (weird) name)");
        let stat = parse_stat(&raw).unwrap();
        assert_eq!(stat.name, "my (weird) name");
        assert_eq!(stat.ppid, 1);
        assert_eq!(stat.start_ticks, 98765);
    }

    #[test]
    fn stat_negative_nice_parses() {
        let raw = NGINX_STAT.replace(" 20 0 3 0 ", " 0 -20 3 0 ");
        assert_eq!(parse_stat(&raw).unwrap().nice, -20);
    }

    #[test]
    fn kernel_thread_flag_is_detected() {
        let raw = "2 (kthreadd) S 0 0 0 0 -1 2129984 0 0 0 0 0 5 0 0 20 0 1 0 2 0 0 18446744073709551615 0 0 0 0 0 0 0 2147483647 0 0 0 0 0 0 0 0 0 0";
        let stat = parse_stat(raw).unwrap();
        assert_ne!(stat.flags & PF_KTHREAD, 0);
        assert_eq!(protection(2, &stat), Some("kernel thread"));
    }

    #[test]
    fn truncated_stat_is_rejected() {
        assert!(parse_stat("1234 (nginx) S 1 1234").is_none());
        assert!(parse_stat("garbage").is_none());
    }

    #[test]
    fn status_yields_effective_uid() {
        let raw = "Name:\tsudo\nUid:\t1000\t0\t0\t0\nGid:\t1000\t1000\t1000\t1000\n";
        assert_eq!(parse_status_euid(raw), Some(0));
    }

    #[test]
    fn cgroup_container_id_both_drivers() {
        let id = "a".repeat(64);
        let v2_systemd = format!("0::/system.slice/docker-{id}.scope\n");
        assert_eq!(parse_cgroup_container(&v2_systemd), Some(id.clone()));
        let v1_cgroupfs = format!("12:memory:/docker/{id}\n11:cpu:/docker/{id}\n");
        assert_eq!(parse_cgroup_container(&v1_cgroupfs), Some(id));
        assert_eq!(
            parse_cgroup_container("0::/user.slice/user-1000.slice\n"),
            None
        );
    }

    #[test]
    fn boot_time_parses() {
        assert_eq!(
            parse_boot_time("cpu 1 2 3\nbtime 1700000000\n"),
            Some(1_700_000_000)
        );
    }

    #[test]
    fn cpu_share_is_per_core() {
        // Half of all ticks across 4 cores = two full cores.
        assert!((cpu_share(50, 100, 4) - 200.0).abs() < f64::EPSILON);
        assert_eq!(cpu_share(10, 0, 4), 0.0);
    }

    #[test]
    fn signal_names_parse_with_and_without_prefix() {
        assert_eq!(Signal::from_wire("term"), Some(Signal::Term));
        assert_eq!(Signal::from_wire("SIGKILL"), Some(Signal::Kill));
        assert_eq!(Signal::from_wire("Hup"), Some(Signal::Hup));
        assert_eq!(Signal::from_wire("segv"), None);
    }

    #[test]
    fn init_and_self_are_protected() {
        let stat = parse_stat(NGINX_STAT).unwrap();
        assert!(protection(1, &stat).is_some());
        assert!(protection(std::process::id(), &stat).is_some());
        assert!(protection(1234, &stat).is_none());
    }

    #[test]
    fn signal_refuses_protected_and_invalid_pids() {
        assert!(matches!(
            signal(1, Signal::Term, None),
            Err(ProcessError::Protected { pid: 1, .. })
        ));
        assert!(matches!(
            signal(std::process::id(), Signal::Term, None),
            Err(ProcessError::Protected { .. })
        ));
        assert!(matches!(
            signal(0, Signal::Term, None),
            Err(ProcessError::InvalidPid(0))
        ));
        assert!(matches!(
            signal(u32::MAX, Signal::Term, None),
            Err(ProcessError::InvalidPid(_))
        ));
    }

    #[test]
    fn signal_refuses_a_reused_pid() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        let start = parse_stat(&fs::read_to_string(format!("/proc/{pid}/stat")).unwrap())
            .unwrap()
            .start_ticks;
        assert!(matches!(
            signal(pid, Signal::Term, Some(start + 1)),
            Err(ProcessError::Replaced(_))
        ));
        signal(pid, Signal::Term, Some(start)).unwrap();
        let status = child.wait().unwrap();
        assert!(!status.success());
    }

    #[test]
    fn live_list_contains_this_process() {
        let list = list(false);
        let me = list
            .processes
            .iter()
            .find(|p| p.pid == std::process::id())
            .expect("own pid in the list");
        assert!(me.protected_reason.is_some());
        assert!(!me.kernel_thread);
        assert!(me.rss_bytes > 0);
        assert!(list.cpu_count >= 1);
        assert!(list.memory_total_bytes > 0);
        assert!(list.processes.iter().all(|p| !p.kernel_thread));
    }
}
