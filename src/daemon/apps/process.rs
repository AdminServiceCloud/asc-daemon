//! Process driver: supervises a plain process via pid-file.
//!
//! Layout inside the app directory: `app.pid` (PID of the spawned process),
//! `app.log` (combined stdout+stderr). The process is detached into its own
//! process group so it survives the CLI/daemon exiting.
//!
//! Known MVP limitation: a PID can be reused by the OS after a reboot, so a
//! stale pid-file may briefly point at a foreign process; the reconcile pass
//! and SIGTERM-before-SIGKILL keep the damage window minimal. A proper
//! supervisor (daemon-held child handles) arrives with the daemon API work.

use std::fs;
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::driver::{AppDriver, ResourceUsage, RuntimeState, tail_lines};
use super::meta::{AppMeta, Runtime};

const PID_FILE: &str = "app.pid";
const LOG_FILE: &str = "app.log";
/// How long to wait for graceful termination before SIGKILL.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

pub struct ProcessDriver;

fn command_of(meta: &AppMeta) -> Result<(&str, &[String])> {
    match &meta.runtime {
        Runtime::Process { command, args } => Ok((command, args)),
        other => bail!("app '{}' is not a process app ({})", meta.id, other.kind()),
    }
}

fn read_pid(dir: &Path) -> Result<Option<u32>> {
    match fs::read_to_string(dir.join(PID_FILE)) {
        Ok(raw) => Ok(raw.trim().parse::<u32>().ok()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).context("cannot read pid file"),
    }
}

/// Whether a process with this PID is alive (`kill(pid, 0)`).
fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 performs only an existence/permission check.
    let res = unsafe { libc::kill(pid as libc::pid_t, 0) };
    // EPERM means the process exists but belongs to someone else.
    res == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn send_signal(pid: u32, signal: libc::c_int) -> Result<()> {
    // SAFETY: plain kill(2) call; errors are checked below.
    if unsafe { libc::kill(pid as libc::pid_t, signal) } != 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ESRCH) {
            return Err(err).with_context(|| format!("cannot signal process {pid}"));
        }
    }
    Ok(())
}

/// CPU time (utime + stime) of `/proc/<pid>/stat` in microseconds.
///
/// The command name (field 2) may contain spaces and parentheses, so parsing
/// starts after the last `)`; utime/stime are then fields 12 and 13 (0-based).
fn parse_proc_stat_cpu_micros(stat: &str, ticks_per_sec: u64) -> Option<u64> {
    let after_comm = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some((utime + stime) * 1_000_000 / ticks_per_sec.max(1))
}

/// Resident memory of `/proc/<pid>/statm` in bytes (field 1 is RSS pages).
fn parse_proc_statm_rss_bytes(statm: &str, page_size: u64) -> Option<u64> {
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * page_size)
}

/// `read_bytes`/`write_bytes` of `/proc/<pid>/io` — actual bytes fetched
/// from/handed off to the storage layer, not `rchar`/`wchar` (which also
/// count page-cache hits). Either may be missing if the kernel lacks
/// `CONFIG_TASK_IO_ACCOUNTING`.
fn parse_proc_io_bytes(raw: &str) -> (Option<u64>, Option<u64>) {
    let mut read = None;
    let mut write = None;
    for line in raw.lines() {
        if let Some(v) = line.strip_prefix("read_bytes:") {
            read = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("write_bytes:") {
            write = v.trim().parse().ok();
        }
    }
    (read, write)
}

/// Resource counters of the main process. Children are not aggregated in the
/// MVP (a supervised process app is a single process by design). Network I/O
/// is not exposed per-process on Linux without a dedicated network
/// namespace, so it is always `None` here.
fn usage_of_pid(pid: u32) -> Option<ResourceUsage> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let statm = fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
    // SAFETY: sysconf has no preconditions; both variables always exist on Linux.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1) as u64;
    let (disk_read_bytes, disk_write_bytes) = fs::read_to_string(format!("/proc/{pid}/io"))
        .ok()
        .map(|raw| parse_proc_io_bytes(&raw))
        .unwrap_or((None, None));
    Some(ResourceUsage {
        cpu_time_micros: parse_proc_stat_cpu_micros(&stat, ticks)?,
        memory_bytes: parse_proc_statm_rss_bytes(&statm, page)?,
        disk_read_bytes,
        disk_write_bytes,
        net_rx_bytes: None,
        net_tx_bytes: None,
    })
}

impl AppDriver for ProcessDriver {
    fn start(&self, meta: &AppMeta, dir: &Path) -> Result<()> {
        let (command, args) = command_of(meta)?;
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(LOG_FILE))
            .context("cannot open app log file")?;
        // Run from repository/ when the package has one, else the app dir.
        let workdir = if dir.join("repository").is_dir() {
            dir.join("repository")
        } else {
            dir.to_path_buf()
        };
        let mut cmd = Command::new(command);
        cmd.args(args)
            .current_dir(workdir)
            .stdin(Stdio::null())
            .stdout(log.try_clone().context("cannot clone log handle")?)
            .stderr(log);
        {
            use std::os::unix::process::CommandExt;
            // Own process group: the app is not killed with the CLI/daemon.
            cmd.process_group(0);
        }
        let child = cmd
            .spawn()
            .with_context(|| format!("cannot start '{command}'"))?;
        fs::write(dir.join(PID_FILE), child.id().to_string()).context("cannot write pid file")?;
        Ok(())
    }

    fn stop(&self, meta: &AppMeta, dir: &Path) -> Result<()> {
        let _ = command_of(meta)?;
        let Some(pid) = read_pid(dir)? else {
            return Ok(());
        };
        if alive(pid) {
            send_signal(pid, libc::SIGTERM)?;
            let deadline = std::time::Instant::now() + STOP_TIMEOUT;
            while alive(pid) && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(200));
            }
            if alive(pid) {
                send_signal(pid, libc::SIGKILL)?;
            }
        }
        fs::remove_file(dir.join(PID_FILE)).ok();
        Ok(())
    }

    fn state(&self, meta: &AppMeta, dir: &Path) -> Result<RuntimeState> {
        let _ = command_of(meta)?;
        match read_pid(dir)? {
            Some(pid) if alive(pid) => Ok(RuntimeState::Running),
            _ => Ok(RuntimeState::Stopped),
        }
    }

    fn usage(&self, meta: &AppMeta, dir: &Path) -> Result<Option<ResourceUsage>> {
        let _ = command_of(meta)?;
        match read_pid(dir)? {
            Some(pid) if alive(pid) => Ok(usage_of_pid(pid)),
            _ => Ok(None),
        }
    }

    fn logs(&self, meta: &AppMeta, dir: &Path, tail: usize) -> Result<String> {
        let _ = command_of(meta)?;
        match fs::read_to_string(dir.join(LOG_FILE)) {
            Ok(text) => Ok(tail_lines(&text, tail)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e).context("cannot read app log file"),
        }
    }

    fn remove(&self, meta: &AppMeta, dir: &Path) -> Result<()> {
        // Make sure nothing keeps running from the removed directory.
        self.stop(meta, dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_stat_cpu_survives_parens_in_comm() {
        // comm "(a b) c)" is hostile but legal; utime=300 stime=100 ticks.
        let stat = "1234 ((a b) c)) S 1 1234 1234 0 -1 4194304 500 0 0 0 300 100 0 0 20 0 1 0 100 1000000 200 18446744073709551615";
        assert_eq!(
            parse_proc_stat_cpu_micros(stat, 100),
            Some((300 + 100) * 10_000)
        );
        assert_eq!(parse_proc_stat_cpu_micros("garbage", 100), None);
    }

    #[test]
    fn proc_statm_rss() {
        assert_eq!(
            parse_proc_statm_rss_bytes("2500 640 300 5 0 800 0", 4096),
            Some(640 * 4096)
        );
        assert_eq!(parse_proc_statm_rss_bytes("", 4096), None);
    }

    #[test]
    fn proc_io_bytes_parses_read_and_write() {
        let raw = "rchar: 9999\nwchar: 8888\nsyscr: 10\nsyscw: 11\n\
                    read_bytes: 4096\nwrite_bytes: 8192\ncancelled_write_bytes: 0\n";
        assert_eq!(parse_proc_io_bytes(raw), (Some(4096), Some(8192)));
        assert_eq!(parse_proc_io_bytes(""), (None, None));
    }
}
