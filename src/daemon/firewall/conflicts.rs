//! Other firewall front-ends on the host (DMN-148). ufw and firewalld both
//! program nftables (or iptables) themselves; a second owner with a `drop`
//! policy silently overrides whatever the daemon accepts, so enabling the
//! managed firewall next to an active one is refused until the operator says
//! to switch the other one off.

use anyhow::{Result, bail};
use serde::Serialize;

use crate::daemon::exec::{has_command, run_captured};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Conflict {
    /// `ufw` or `firewalld`.
    pub kind: String,
    pub detail: String,
}

pub fn ufw_active(status_output: &str) -> bool {
    status_output
        .lines()
        .any(|line| line.trim().eq_ignore_ascii_case("status: active"))
}

fn unit_active(unit: &str) -> bool {
    run_captured("systemctl", &["is-active", unit])
        .map(|(ok, out)| ok && out.trim() == "active")
        .unwrap_or(false)
}

/// Active competing firewall managers.
pub fn detect() -> Vec<Conflict> {
    let mut out = Vec::new();
    if has_command("ufw")
        && run_captured("ufw", &["status"])
            .map(|(_, text)| ufw_active(&text))
            .unwrap_or(false)
    {
        out.push(Conflict {
            kind: "ufw".into(),
            detail: "ufw is active and manages its own rules".into(),
        });
    }
    if has_command("firewall-cmd") && unit_active("firewalld") {
        out.push(Conflict {
            kind: "firewalld".into(),
            detail: "firewalld is running and manages its own rules".into(),
        });
    }
    out
}

/// Switches the given managers off (`force`). Their rules are left to them:
/// nothing is merged into the daemon's table.
pub fn disable(conflicts: &[Conflict]) -> Result<()> {
    for conflict in conflicts {
        match conflict.kind.as_str() {
            "ufw" => {
                let (ok, out) = run_captured("ufw", &["--force", "disable"])?;
                if !ok {
                    bail!("cannot disable ufw: {}", out.trim());
                }
            }
            "firewalld" => {
                let (ok, out) = run_captured("systemctl", &["disable", "--now", "firewalld"])?;
                if !ok {
                    bail!("cannot disable firewalld: {}", out.trim());
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ufw_status_is_recognised() {
        assert!(ufw_active("Status: active\n\nTo   Action  From\n"));
        assert!(!ufw_active("Status: inactive\n"));
        assert!(!ufw_active(""));
    }
}
