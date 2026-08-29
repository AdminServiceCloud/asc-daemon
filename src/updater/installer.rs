//! Install / update / rollback of the `asc` daemon binary.
//!
//! The updater is deliberately independent from the daemon: it can replace
//! and restart a broken `asc` because it never links against daemon state —
//! only the shared config. The updater never overwrites itself; the daemon
//! updates the updater (mutual updates, no single point of failure).

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use semver::Version;
use serde::Serialize;
use tracing::{debug, warn};

use asc_daemon::daemon::config::Config;
use asc_daemon::daemon::i18n::{Msg, t, tf, tf2};

use super::github::{self, Release};

/// A single machine-readable `asc-updater update --json` line. Stage names
/// are a stable interface consumed by nodeservice; human output remains
/// localized and is emitted only by [`update`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateEvent {
    #[serde(rename = "type")]
    pub event_type: &'static str,
    pub stage: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percent: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub message: String,
}

impl UpdateEvent {
    fn progress(
        stage: &'static str,
        percent: u8,
        version: Option<&str>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            event_type: "progress",
            stage,
            percent: Some(percent),
            version: version.map(str::to_owned),
            message: message.into(),
        }
    }

    fn complete(version: Option<&str>, message: impl Into<String>) -> Self {
        Self {
            event_type: "complete",
            stage: "completed",
            percent: Some(100),
            version: version.map(str::to_owned),
            message: message.into(),
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            event_type: "error",
            stage: "error",
            percent: None,
            version: None,
            message: message.into(),
        }
    }
}

/// Rust target triple this binary was built for — release assets are named
/// after it. `None` on platforms we do not publish builds for.
pub fn target_triple() -> Option<&'static str> {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("x86_64-unknown-linux-gnu")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some("aarch64-unknown-linux-gnu")
    } else if cfg!(all(target_os = "linux", target_arch = "arm")) {
        Some("armv7-unknown-linux-gnueabihf")
    } else {
        None
    }
}

/// Where the previous `asc` binary is kept for `rollback`.
fn previous_binary_path(config: &Config) -> PathBuf {
    config.daemon.data_dir.join("updater").join("asc.previous")
}

fn installed_asc(config: &Config) -> PathBuf {
    config.updater.install_dir.join("asc")
}

/// Version of the installed daemon (`asc --version` → `asc 0.1.0`).
pub fn installed_version(config: &Config) -> Option<String> {
    let out = Command::new(installed_asc(config))
        .arg("--version")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    raw.split_whitespace().last().map(str::to_string)
}

fn parse_version(raw: &str) -> Result<Version> {
    let normalized = raw.strip_prefix('v').unwrap_or(raw);
    Version::parse(normalized).with_context(|| format!("invalid semantic version '{raw}'"))
}

/// Whether `available` is a valid SemVer release strictly newer than the
/// installed version. A leading `v` is accepted on either side.
pub fn is_strictly_newer(installed: &str, available: &str) -> Result<bool> {
    Ok(parse_version(available)? > parse_version(installed)?)
}

/// Download the release tarball for this platform, verify its checksum and
/// return the contained `asc` binary.
fn fetch_daemon_binary(release: &Release) -> Result<Vec<u8>> {
    fetch_daemon_binary_inner(release, true, &mut |_| {})
}

fn fetch_daemon_binary_inner<F>(
    release: &Release,
    human_output: bool,
    on_event: &mut F,
) -> Result<Vec<u8>>
where
    F: FnMut(UpdateEvent),
{
    let Some(triple) = target_triple() else {
        bail!(tf2(
            Msg::UpdNoBuildForPlatform,
            &release.tag_name,
            std::env::consts::ARCH
        ));
    };
    let tar_name = format!("asc-{}-{}.tar.gz", release.tag_name, triple);
    let sha_name = format!("asc-{}-{}.sha256", release.tag_name, triple);
    let tar_asset = release.asset(&tar_name).ok_or_else(|| {
        anyhow::anyhow!(tf2(Msg::UpdNoBuildForPlatform, &release.tag_name, triple))
    })?;
    let sha_asset = release.asset(&sha_name).with_context(|| {
        format!(
            "release {} has no checksum file {sha_name}",
            release.tag_name
        )
    })?;

    if human_output {
        println!("{}", tf(Msg::UpdDownloading, &tar_name));
    }
    on_event(UpdateEvent::progress(
        "downloading",
        15,
        Some(&release.tag_name),
        "Downloading release archive",
    ));
    let tarball = github::download(tar_asset)?;
    let checksum = String::from_utf8(github::download(sha_asset)?)
        .context("checksum file is not valid UTF-8")?;
    on_event(UpdateEvent::progress(
        "verifying_checksum",
        40,
        Some(&release.tag_name),
        "Verifying release checksum",
    ));
    github::verify_sha256(&tarball, &checksum, &tar_name)?;
    extract_from_tar_gz(&tarball, "asc")
}

/// Pull one file out of a `.tar.gz` archive.
pub fn extract_from_tar_gz(data: &[u8], wanted: &str) -> Result<Vec<u8>> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(data));
    for entry in archive.entries().context("invalid release archive")? {
        let mut entry = entry.context("invalid release archive entry")?;
        let path = entry.path().context("invalid path in release archive")?;
        if path.as_os_str() == wanted {
            let mut out = Vec::new();
            entry
                .read_to_end(&mut out)
                .context("cannot read file from release archive")?;
            return Ok(out);
        }
    }
    bail!("release archive has no '{wanted}' binary")
}

/// Write a binary atomically (tmp + rename) with exec permissions; safe to
/// call over a running binary on Unix (the old inode stays alive).
fn write_binary(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path.parent().context("binary path has no parent")?;
    fs::create_dir_all(dir)
        .with_context(|| format!("cannot create directory {}", dir.display()))?;
    let tmp = dir.join(".asc.tmp");
    fs::write(&tmp, data).with_context(|| format!("cannot write {}", tmp.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))
            .context("cannot set exec permissions")?;
    }
    fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

/// Install the daemon: download, place the binary, persist config, register
/// the systemd service and the auto-update timer.
pub fn install(config: &Config) -> Result<()> {
    let release = github::latest_release(config.updater.channel)?;
    let binary = fetch_daemon_binary(&release)?;
    write_binary(&installed_asc(config), &binary)?;
    config.save()?;
    install_completions(config);
    setup_daemon_service(config);
    if config.updater.enabled
        && let Err(err) = setup_timer(config)
    {
        warn!(error = %format!("{err:#}"), "cannot set up auto-update timer");
    }
    println!("{}", tf(Msg::UpdInstalled, &release.tag_name));
    Ok(())
}

/// Update to the channel's latest release, keeping the old binary for rollback.
pub fn update(config: &Config, _force: bool) -> Result<()> {
    update_inner(config, true, &mut |_| {})
}

/// Update with NDJSON-compatible progress events and no human-readable stdout.
pub fn update_with_progress<F>(config: &Config, _force: bool, mut on_event: F) -> Result<()>
where
    F: FnMut(UpdateEvent),
{
    update_inner(config, false, &mut on_event)
}

fn update_inner<F>(config: &Config, human_output: bool, on_event: &mut F) -> Result<()>
where
    F: FnMut(UpdateEvent),
{
    // TODO(DMN-005): once the daemon API exists, ask it for active tasks
    // (install, backup) and postpone the update unless --force.
    on_event(UpdateEvent::progress(
        "checking",
        0,
        None,
        "Checking available daemon version",
    ));
    let release = github::latest_release(config.updater.channel)?;
    let installed = installed_version(config)
        .context("daemon is not installed or its version cannot be determined")?;
    if !is_strictly_newer(&installed, &release.tag_name)? {
        if human_output {
            println!("{}", tf(Msg::UpdUpToDate, &installed));
        }
        on_event(UpdateEvent::complete(
            Some(&installed),
            "No newer daemon version is available",
        ));
        return Ok(());
    }
    let binary = fetch_daemon_binary_inner(&release, human_output, on_event)?;

    let current = installed_asc(config);
    on_event(UpdateEvent::progress(
        "backing_up",
        55,
        Some(&release.tag_name),
        "Saving the currently installed daemon",
    ));
    if current.exists() {
        let previous = previous_binary_path(config);
        fs::create_dir_all(previous.parent().expect("has parent"))
            .context("cannot create updater state directory")?;
        fs::copy(&current, &previous).context("cannot save previous version for rollback")?;
    }
    on_event(UpdateEvent::progress(
        "replacing",
        75,
        Some(&release.tag_name),
        "Replacing the daemon binary",
    ));
    write_binary(&current, &binary)?;
    install_completions(config);
    on_event(UpdateEvent::progress(
        "restarting",
        90,
        Some(&release.tag_name),
        "Restarting the daemon service",
    ));
    restart_daemon();
    if human_output {
        println!("{}", tf(Msg::UpdUpdated, &release.tag_name));
    }
    on_event(UpdateEvent::complete(
        Some(&release.tag_name),
        "Daemon update completed",
    ));
    Ok(())
}

/// Swap the current binary with the saved previous one.
pub fn rollback(config: &Config) -> Result<()> {
    let previous = previous_binary_path(config);
    if !previous.exists() {
        bail!(t(Msg::UpdNoPrevious));
    }
    let current = installed_asc(config);
    let previous_data = fs::read(&previous).context("cannot read previous version")?;
    // Keep the (bad) current version so a rollback can itself be rolled back.
    let current_data = fs::read(&current).ok();
    write_binary(&current, &previous_data)?;
    match current_data {
        Some(data) => fs::write(&previous, data).context("cannot save replaced version")?,
        None => fs::remove_file(&previous).context("cannot remove previous version")?,
    }
    restart_daemon();
    println!("{}", t(Msg::UpdRolledBack));
    Ok(())
}

/// Where each shell looks for a system-wide completion script, in preference
/// order (DMN-055). Only directories that already exist are written to: the
/// presence of the directory is what says the shell (and, for bash, the
/// bash-completion package) is installed on this host. Debian/Ubuntu ship the
/// first entry of each list; the others cover distributions and manual
/// installs that place them elsewhere.
const COMPLETION_TARGETS: &[(&str, &str, &[&str])] = &[
    (
        "bash",
        "asc",
        &[
            "/usr/share/bash-completion/completions",
            "/etc/bash_completion.d",
        ],
    ),
    (
        "zsh",
        "_asc",
        &[
            "/usr/share/zsh/vendor-completions",
            "/usr/share/zsh/site-functions",
            "/usr/local/share/zsh/site-functions",
        ],
    ),
    (
        "fish",
        "asc.fish",
        &[
            "/usr/share/fish/vendor_completions.d",
            "/usr/local/share/fish/vendor_completions.d",
        ],
    ),
];

/// Drop the shell completion scripts next to the freshly installed binary
/// (DMN-055). The scripts come from that binary (`asc completion <shell>`),
/// not from the updater's own copy, so an updated `asc` always ships the
/// script version it expects.
///
/// Best-effort throughout: a host without zsh, a read-only `/usr`, an `asc`
/// too old to know the command — none of that is a reason to fail an install
/// or an update. Tab completion is a convenience, the binary is the product.
fn install_completions(config: &Config) {
    let asc = installed_asc(config);
    for (shell, file_name, dirs) in COMPLETION_TARGETS {
        let Some(dir) = dirs.iter().map(Path::new).find(|dir| dir.is_dir()) else {
            continue;
        };
        let script = match Command::new(&asc).args(["completion", shell]).output() {
            Ok(out) if out.status.success() => out.stdout,
            Ok(out) => {
                debug!(
                    shell,
                    status = ?out.status.code(),
                    "asc completion failed; skipping this shell"
                );
                continue;
            }
            Err(err) => {
                warn!(error = %err, "cannot run asc to generate completions");
                return;
            }
        };
        let path = dir.join(file_name);
        // Completion scripts are world-readable data, not executables: the
        // shell sources them, and 0644 is what every distro ships.
        if let Err(err) = fs::write(&path, &script) {
            warn!(file = %path.display(), error = %err, "cannot write completion script");
            continue;
        }
        debug!(shell, file = %path.display(), "completion script installed");
    }
}

/// Register the daemon as a service and start it (Linux; skipped elsewhere).
fn setup_daemon_service(config: &Config) {
    if !cfg!(target_os = "linux") {
        warn!("skipping service setup: not Linux");
        return;
    }
    let asc = installed_asc(config);
    for args in [&["service", "install"][..], &["service", "start"][..]] {
        match Command::new(&asc).args(args).output() {
            Ok(out) if out.status.success() => {}
            Ok(out) => warn!(
                args = ?args,
                error = %String::from_utf8_lossy(&out.stderr).trim(),
                "asc service setup step failed"
            ),
            Err(err) => warn!(args = ?args, error = %err, "cannot run asc"),
        }
    }
}

fn restart_daemon() {
    if !cfg!(target_os = "linux") {
        return;
    }
    if let Err(err) = systemctl(&["restart", "asc"]) {
        // The daemon may simply not be installed as a service yet.
        warn!(error = %format!("{err:#}"), "cannot restart asc service");
    }
}

// ── Auto-update timer (systemd) ─────────────────────────────────────────────

const TIMER_UNIT: &str = "asc-updater.timer";
const SERVICE_PATH: &str = "/etc/systemd/system/asc-updater.service";
const TIMER_PATH: &str = "/etc/systemd/system/asc-updater.timer";

/// Unit file contents for the daily update check.
pub fn timer_units(updater_path: &Path, schedule: &str) -> (String, String) {
    let service = format!(
        r#"# Generated by asc-updater — manual edits will be overwritten.
[Unit]
Description=ASC daemon update check
Documentation=https://github.com/{repo}

[Service]
Type=oneshot
ExecStart={updater} update
"#,
        repo = github::REPO,
        updater = updater_path.display()
    );
    let timer = format!(
        r#"# Generated by asc-updater — manual edits will be overwritten.
[Unit]
Description=Daily ASC daemon update check

[Timer]
OnCalendar=*-*-* {schedule}:00
RandomizedDelaySec=15m
Persistent=true

[Install]
WantedBy=timers.target
"#
    );
    (service, timer)
}

/// Install and enable the auto-update timer.
pub fn setup_timer(config: &Config) -> Result<()> {
    if !cfg!(target_os = "linux") {
        warn!("skipping auto-update timer: not Linux");
        return Ok(());
    }
    let updater = std::env::current_exe().context("cannot resolve current executable")?;
    let (service, timer) = timer_units(&updater, &config.updater.schedule);
    fs::write(SERVICE_PATH, service).with_context(|| format!("cannot write {SERVICE_PATH}"))?;
    fs::write(TIMER_PATH, timer).with_context(|| format!("cannot write {TIMER_PATH}"))?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", TIMER_UNIT])?;
    Ok(())
}

/// Disable the timer (manual updates only).
pub fn disable_timer() -> Result<()> {
    if !cfg!(target_os = "linux") {
        return Ok(());
    }
    systemctl(&["disable", "--now", TIMER_UNIT])
}

fn systemctl(args: &[&str]) -> Result<()> {
    let out = Command::new("systemctl")
        .args(args)
        .output()
        .context("cannot run systemctl")?;
    if !out.status.success() {
        bail!(
            "systemctl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;

    fn make_tar_gz(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
        for (name, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, name, *data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn extracts_binary_from_tarball() {
        let tarball = make_tar_gz(&[("asc", b"daemon-bytes"), ("asc-updater", b"updater")]);
        assert_eq!(
            extract_from_tar_gz(&tarball, "asc").unwrap(),
            b"daemon-bytes"
        );
        assert!(extract_from_tar_gz(&tarball, "missing").is_err());
    }

    #[test]
    fn version_comparison_is_semver_and_strictly_newer() {
        assert!(is_strictly_newer("v1.2.0", "1.2.1").unwrap());
        assert!(!is_strictly_newer("1.2.0", "v1.2.0").unwrap());
        assert!(!is_strictly_newer("v1.2.0", "v1.1.9").unwrap());

        assert!(is_strictly_newer("1.0.0-beta.1", "v1.0.0-beta.2").unwrap());
        assert!(is_strictly_newer("v1.0.0-rc.1", "1.0.0").unwrap());
        assert!(!is_strictly_newer("1.0.0", "1.0.0-rc.1").unwrap());

        assert!(is_strictly_newer("not-semver", "1.0.0").is_err());
        assert!(is_strictly_newer("1.0.0", "latest").is_err());
    }

    #[test]
    fn update_event_has_stable_machine_shape() {
        let event = UpdateEvent::progress("verifying_checksum", 40, Some("v1.2.3"), "Verified");
        let json = serde_json::to_value(event).unwrap();
        assert_eq!(json["type"], "progress");
        assert_eq!(json["stage"], "verifying_checksum");
        assert_eq!(json["percent"], 40);
        assert_eq!(json["version"], "v1.2.3");
        assert_eq!(json["message"], "Verified");

        let error = serde_json::to_value(UpdateEvent::error("safe failure")).unwrap();
        assert_eq!(error["type"], "error");
        assert_eq!(error["stage"], "error");
        assert!(error.get("percent").is_none());
        assert!(error.get("version").is_none());
    }

    #[test]
    fn timer_units_contain_schedule_and_path() {
        let (service, timer) = timer_units(Path::new("/usr/local/bin/asc-updater"), "04:00");
        assert!(service.contains("ExecStart=/usr/local/bin/asc-updater update"));
        assert!(timer.contains("OnCalendar=*-*-* 04:00:00"));
        assert!(timer.contains("Persistent=true"));
    }

    #[test]
    fn atomic_binary_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bin").join("asc");
        write_binary(&path, b"one").unwrap();
        write_binary(&path, b"two").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        assert!(!dir.path().join("bin").join(".asc.tmp").exists());
    }
}
