//! Service management for the daemon (`asc service ...`).
//!
//! Init-system specifics live behind the [`ServiceManager`] trait: systemd is
//! the first implementation, other init systems and launchd (macOS) plug in
//! later without touching the CLI.

pub mod systemd;

use anyhow::{Result, bail};

use crate::daemon::i18n::{Msg, t};

/// State of the installed daemon service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    Active,
    Inactive,
    NotInstalled,
}

/// Abstraction over the host init system.
pub trait ServiceManager {
    /// Install the service unit and enable autostart.
    fn install(&self) -> Result<()>;
    /// Stop, disable and remove the service unit.
    fn uninstall(&self) -> Result<()>;
    fn start(&self) -> Result<()>;
    fn stop(&self) -> Result<()>;
    fn restart(&self) -> Result<()>;
    fn state(&self) -> Result<ServiceState>;
}

/// Detect the init system of the host and return its manager.
pub fn detect() -> Result<Box<dyn ServiceManager>> {
    #[cfg(target_os = "linux")]
    {
        if systemd::available() {
            return Ok(Box::new(systemd::Systemd));
        }
        bail!(t(Msg::ErrNoSystemd));
    }
    #[cfg(not(target_os = "linux"))]
    {
        bail!(t(Msg::ErrUnsupportedOs));
    }
}

/// Fail with a user-friendly error when not running as root.
pub fn require_root() -> Result<()> {
    // SAFETY: geteuid() has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        bail!(t(Msg::ErrRootRequired));
    }
    Ok(())
}
