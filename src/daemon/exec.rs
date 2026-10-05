//! Running host commands for the system modules (web server, firewall,
//! fail2ban): captured or with every output line forwarded to a progress
//! sink, plus the little host facts those modules need (`PATH` lookup,
//! `/etc/os-release`). Blocking; callers run it from worker threads.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;

use anyhow::{Context, Result, bail};

/// Progress sink for long operations: one human line at a time.
pub type Progress<'a> = &'a mut dyn FnMut(&str);

/// Errors the system modules (firewall, fail2ban) raise on purpose, so the
/// API layers can answer with the right status instead of "internal".
#[derive(Debug)]
pub enum ModuleError {
    /// The caller sent something unusable.
    Invalid(String),
    /// The request is fine but the module is not in a state to do it.
    Precondition(String),
    NotFound(String),
}

impl std::fmt::Display for ModuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(m) | Self::Precondition(m) | Self::NotFound(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for ModuleError {}

pub fn invalid(message: impl std::fmt::Display) -> anyhow::Error {
    ModuleError::Invalid(message.to_string()).into()
}

pub fn precondition(message: impl std::fmt::Display) -> anyhow::Error {
    ModuleError::Precondition(message.to_string()).into()
}

pub fn not_found(message: impl std::fmt::Display) -> anyhow::Error {
    ModuleError::NotFound(message.to_string()).into()
}

/// Runs a command, forwarding every output line to `progress`.
pub fn run_streaming(cmd: &str, args: &[&str], progress: Progress<'_>) -> Result<()> {
    progress(&format!("$ {cmd} {}", args.join(" ")));
    let mut child = Command::new(cmd)
        .args(args)
        .env("DEBIAN_FRONTEND", "noninteractive")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot run {cmd}"))?;
    let (tx, rx) = mpsc::channel::<String>();
    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        let tx = tx.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        }));
    }
    if let Some(err) = child.stderr.take() {
        let tx = tx.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        }));
    }
    drop(tx);
    for line in rx {
        if !line.trim().is_empty() {
            progress(&line);
        }
    }
    for reader in readers {
        let _ = reader.join();
    }
    let status = child
        .wait()
        .with_context(|| format!("{cmd} did not finish"))?;
    if !status.success() {
        bail!("{cmd} {} failed with {status}", args.join(" "));
    }
    Ok(())
}

/// Runs a command and returns (success, stdout+stderr).
pub fn run_captured(cmd: &str, args: &[&str]) -> Result<(bool, String)> {
    let out = Command::new(cmd)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("cannot run {cmd}"))?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok((out.status.success(), text))
}

/// Runs a command with `input` on its stdin and returns (success,
/// stdout+stderr) — for `nft -f -` and `nft -c -f -`, where the ruleset
/// comes from memory, not from a file.
pub fn run_with_input(cmd: &str, args: &[&str], input: &str) -> Result<(bool, String)> {
    use std::io::Write;
    let mut child = Command::new(cmd)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot run {cmd}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        // A command that exits early closes the pipe; its own error output
        // is the useful message then, so the write result is not fatal.
        let _ = stdin.write_all(input.as_bytes());
    }
    let out = child
        .wait_with_output()
        .with_context(|| format!("{cmd} did not finish"))?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok((out.status.success(), text))
}

/// Whether `name` is an executable file on `PATH`.
pub fn has_command(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
}

/// The fields of `/etc/os-release` the installers care about.
#[derive(Debug, Default, Clone)]
pub struct OsRelease {
    pub id: String,
    pub id_like: String,
    pub codename: String,
}

pub fn os_release() -> OsRelease {
    let text = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let mut out = OsRelease::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"').to_ascii_lowercase();
        match key.trim() {
            "ID" => out.id = value,
            "ID_LIKE" => out.id_like = value,
            "VERSION_CODENAME" => out.codename = value,
            "UBUNTU_CODENAME" if out.codename.is_empty() => out.codename = value,
            _ => {}
        }
    }
    out
}

impl OsRelease {
    pub fn is(&self, family: &str) -> bool {
        self.id == family || self.id_like.split_whitespace().any(|f| f == family)
    }
}

/// Installs `package` with the host's package manager (apt or dnf/yum),
/// streaming the output. The system modules that only need a distribution
/// package (nftables, fail2ban) share this; the web server has its own
/// repository handling.
pub fn install_package(package: &str, progress: Progress<'_>) -> Result<()> {
    if has_command("apt-get") {
        run_streaming("apt-get", &["update"], progress)?;
        run_streaming(
            "apt-get",
            &[
                "install",
                "-y",
                "-o",
                "Dpkg::Options::=--force-confold",
                package,
            ],
            progress,
        )
    } else if has_command("dnf") {
        run_streaming("dnf", &["install", "-y", package], progress)
    } else if has_command("yum") {
        run_streaming("yum", &["install", "-y", package], progress)
    } else {
        bail!("no supported package manager (apt, dnf, yum) found to install {package}")
    }
}

/// Removes `package`; `purge` also drops its configuration where the package
/// manager supports it.
pub fn remove_package(package: &str, purge: bool, progress: Progress<'_>) -> Result<()> {
    if has_command("apt-get") {
        let verb = if purge { "purge" } else { "remove" };
        run_streaming("apt-get", &[verb, "-y", package], progress)
    } else if has_command("dnf") {
        run_streaming("dnf", &["remove", "-y", package], progress)
    } else if has_command("yum") {
        run_streaming("yum", &["remove", "-y", package], progress)
    } else {
        bail!("no supported package manager (apt, dnf, yum) found to remove {package}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_reports_status_and_output() {
        let (ok, out) = run_captured("sh", &["-c", "echo hi; echo err >&2; exit 0"]).unwrap();
        assert!(ok);
        assert!(out.contains("hi") && out.contains("err"));
        let (ok, _) = run_captured("sh", &["-c", "exit 3"]).unwrap();
        assert!(!ok);
    }

    #[test]
    fn input_reaches_stdin() {
        let (ok, out) = run_with_input("cat", &[], "ruleset text").unwrap();
        assert!(ok);
        assert_eq!(out, "ruleset text");
    }

    #[test]
    fn os_release_family_matches_id_and_id_like() {
        let os = OsRelease {
            id: "ubuntu".into(),
            id_like: "debian".into(),
            codename: String::new(),
        };
        assert!(os.is("ubuntu") && os.is("debian") && !os.is("fedora"));
    }
}
