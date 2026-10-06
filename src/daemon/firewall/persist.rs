//! Keeping the confirmed ruleset across reboots: the script lives
//! in `/etc/asc/firewall/ruleset.nft` and a oneshot systemd unit loads it
//! before the network comes up. The distribution's `/etc/nftables.conf` and
//! `nftables.service` are never touched.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::daemon::exec::run_captured;
use crate::daemon::webserver::write_atomic;

pub const UNIT_NAME: &str = "asc-firewall.service";

/// Where the firewall keeps its files.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `/etc/asc/firewall`: the confirmed ruleset script.
    pub etc: PathBuf,
    /// `/var/lib/asc/firewall`: model, pending change.
    pub state: PathBuf,
    /// Where the systemd unit goes; `None` disables unit management (tests,
    /// hosts without systemd).
    pub unit_dir: Option<PathBuf>,
}

impl Paths {
    pub fn system() -> Self {
        Self {
            etc: PathBuf::from("/etc/asc/firewall"),
            state: PathBuf::from("/var/lib/asc/firewall"),
            unit_dir: Some(PathBuf::from("/etc/systemd/system")),
        }
    }

    pub fn ruleset(&self) -> PathBuf {
        self.etc.join("ruleset.nft")
    }

    pub fn state_file(&self) -> PathBuf {
        self.state.join("state.json")
    }

    pub fn pending_file(&self) -> PathBuf {
        self.state.join("pending.json")
    }
}

/// The unit text. `After=nftables.service` matters: the distribution's own
/// unit may start with `flush ruleset`, and ours has to load after it.
pub fn unit_text(nft: &str, ruleset: &Path) -> String {
    format!(
        "# Added by asc-daemon (firewall): loads the confirmed ruleset at boot.\n\
         [Unit]\n\
         Description=ASC firewall (nftables table inet asc)\n\
         Documentation=https://docs.adminservice.cloud/firewall\n\
         DefaultDependencies=no\n\
         After=local-fs.target nftables.service\n\
         Before=network-pre.target\n\
         Wants=network-pre.target\n\
         ConditionPathExists={ruleset}\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         ExecStart={nft} -f {ruleset}\n\
         \n\
         [Install]\n\
         WantedBy=sysinit.target\n",
        ruleset = ruleset.display(),
    )
}

/// Writes the confirmed script and (re)installs the unit.
pub fn save_ruleset(paths: &Paths, nft_bin: &str, script: &str) -> Result<()> {
    write_atomic(&paths.ruleset(), script.as_bytes(), 0o600)
        .context("cannot save the confirmed ruleset")?;
    let Some(dir) = &paths.unit_dir else {
        return Ok(());
    };
    let unit = dir.join(UNIT_NAME);
    let text = unit_text(nft_bin, &paths.ruleset());
    let current = std::fs::read_to_string(&unit).unwrap_or_default();
    if current != text {
        write_atomic(&unit, text.as_bytes(), 0o644).context("cannot write the systemd unit")?;
        systemctl(&["daemon-reload"])?;
    }
    systemctl(&["enable", UNIT_NAME])?;
    Ok(())
}

/// Forgets the confirmed ruleset: nothing is loaded at the next boot.
pub fn clear_ruleset(paths: &Paths) -> Result<()> {
    if let Some(dir) = &paths.unit_dir {
        let unit = dir.join(UNIT_NAME);
        if unit.exists() {
            let _ = systemctl(&["disable", UNIT_NAME]);
            std::fs::remove_file(&unit).context("cannot remove the systemd unit")?;
            let _ = systemctl(&["daemon-reload"]);
        }
    }
    match std::fs::remove_file(paths.ruleset()) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).context("cannot remove the saved ruleset"),
    }
}

fn systemctl(args: &[&str]) -> Result<()> {
    let (ok, out) = run_captured("systemctl", args)?;
    if !ok {
        bail!("systemctl {} failed: {}", args.join(" "), out.trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_loads_the_saved_ruleset_after_the_distribution_unit() {
        let text = unit_text("/usr/sbin/nft", Path::new("/etc/asc/firewall/ruleset.nft"));
        assert!(text.contains("ExecStart=/usr/sbin/nft -f /etc/asc/firewall/ruleset.nft"));
        assert!(text.contains("After=local-fs.target nftables.service"));
        assert!(text.contains("Before=network-pre.target"));
        assert!(text.contains("ConditionPathExists=/etc/asc/firewall/ruleset.nft"));
    }

    #[test]
    fn saving_and_clearing_without_a_unit_dir_touches_only_the_ruleset() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            etc: dir.path().join("etc"),
            state: dir.path().join("state"),
            unit_dir: None,
        };
        save_ruleset(&paths, "nft", "table inet asc\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(paths.ruleset()).unwrap(),
            "table inet asc\n"
        );
        clear_ruleset(&paths).unwrap();
        assert!(!paths.ruleset().exists());
        clear_ruleset(&paths).unwrap();
    }
}
