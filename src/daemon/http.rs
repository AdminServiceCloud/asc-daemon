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

/// Total time budget per request, seconds. Generous — this path also serves
/// `asc-updater` release-asset downloads, which are larger and may run over
/// slower links.
const MAX_TIME_SECS: &str = "300";
/// Refuse to download files larger than this (bytes).
const MAX_FILESIZE: &str = "536870912"; // 512 MiB

/// GET a URL and return the response body as bytes.
pub fn get_bytes(url: &str) -> Result<Vec<u8>> {
    get_bytes_with_timeout(url, MAX_TIME_SECS)
}

/// GET a URL and return the response body as UTF-8 text.
pub fn get_string(url: &str) -> Result<String> {
    get_string_with_timeout(url, MAX_TIME_SECS)
}

/// `get_bytes` with an explicit total time budget (seconds) instead of the
/// 300s default — small, latency-sensitive fetches (registry indexes) should
/// give up on a stalled connection long before that.
pub fn get_bytes_with_timeout(url: &str, max_time_secs: &str) -> Result<Vec<u8>> {
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
            max_time_secs,
            "--max-filesize",
            MAX_FILESIZE,
            // A stalled/throttled connection fails within max-time instead of
            // hanging silently; retries then recover from the transient hosts
            // that reset or 5xx rather than time out outright.
            "--retry",
            "2",
            "--retry-delay",
            "1",
            "--retry-all-errors",
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

/// `get_string` with an explicit total time budget (seconds); see
/// [`get_bytes_with_timeout`].
pub fn get_string_with_timeout(url: &str, max_time_secs: &str) -> Result<String> {
    String::from_utf8(get_bytes_with_timeout(url, max_time_secs)?)
        .with_context(|| format!("{url}: response is not UTF-8"))
}
