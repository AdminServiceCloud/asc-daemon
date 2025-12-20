//! Minimal HTTPS fetching via the system `curl`.
//!
//! Deliberate design choice for the bootstrap path: the updater and the
//! registry client must stay dependency-light and keep working even when the
//! daemon is broken; `curl` is guaranteed by install.sh on every supported
//! distribution. The daemon API server (DMN-005) brings a real HTTP stack
//! (hyper/rustls) when it lands — this helper is for outbound fetches only.

use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::daemon::i18n::{Msg, t};

/// Total time budget per request, seconds.
const MAX_TIME_SECS: &str = "300";
/// Refuse to download files larger than this (bytes).
const MAX_FILESIZE: &str = "536870912"; // 512 MiB

/// GET a URL and return the response body as bytes.
pub fn get_bytes(url: &str) -> Result<Vec<u8>> {
    let out = match Command::new("curl")
        .args([
            "--proto",
            "=https",
            "--tlsv1.2",
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--max-time",
            MAX_TIME_SECS,
            "--max-filesize",
            MAX_FILESIZE,
            "--user-agent",
            concat!("asc-daemon/", env!("CARGO_PKG_VERSION")),
            url,
        ])
        .output()
    {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(t(Msg::ErrCurlNotFound)),
        Err(e) => return Err(e).context("cannot run curl"),
    };
    if !out.status.success() {
        bail!(
            "cannot fetch {url}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// GET a URL and return the response body as UTF-8 text.
pub fn get_string(url: &str) -> Result<String> {
    String::from_utf8(get_bytes(url)?).with_context(|| format!("{url}: response is not UTF-8"))
}
