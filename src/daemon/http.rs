//! Minimal HTTPS fetching via the system `curl`.
//!
//! Deliberate design choice for the bootstrap path: the updater and the
//! registry client must stay dependency-light and keep working even when the
//! daemon is broken; `curl` is guaranteed by install.sh on every supported
//! distribution. The daemon API server (DMN-005) brings a real HTTP stack
//! (hyper/rustls) when it lands — this helper is for outbound fetches only.

use std::io::Write;
use std::process::{Command, Stdio};

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

/// POST a JSON body and return the response body as UTF-8 text.
///
/// The body goes in on stdin rather than as an argument: registration tokens
/// pass through here, and process arguments are readable by every user on the
/// machine via `ps`.
///
/// Unlike the GET helpers this one also permits `http://` — a platform under
/// local development is reachable that way and refusing it would make the
/// bootstrap path untestable. Plain HTTP is warned about by the caller.
pub fn post_json(url: &str, body: &str) -> Result<String> {
    let mut child = match Command::new("curl")
        .args([
            "--proto",
            "=https,http",
            "--tlsv1.2",
            "--fail-with-body",
            "--silent",
            "--show-error",
            "--location",
            "--max-time",
            "60",
            "--max-filesize",
            MAX_FILESIZE,
            "--header",
            "Content-Type: application/json",
            "--data-binary",
            "@-",
            "--user-agent",
            concat!("asc-daemon/", env!("CARGO_PKG_VERSION")),
            url,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(t(Msg::ErrCurlNotFound)),
        Err(e) => return Err(e).context("cannot run curl"),
    };
    child
        .stdin
        .take()
        .context("cannot open curl stdin")?
        .write_all(body.as_bytes())
        .context("cannot send the request body")?;
    let out = child.wait_with_output().context("cannot run curl")?;
    let response = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() {
        let detail = if response.is_empty() {
            String::from_utf8_lossy(&out.stderr).trim().to_string()
        } else {
            response
        };
        bail!("cannot POST {url}: {detail}");
    }
    Ok(response)
}
