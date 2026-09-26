//! Cloudflare edge ranges for `real_ip` (DMN-125).
//!
//! A domain proxied by Cloudflare reaches nginx from Cloudflare's addresses;
//! the visitor's address travels in `CF-Connecting-IP`. nginx may only trust
//! that header from Cloudflare itself — otherwise any client could claim any
//! address — so the list of ranges is the whole security of the feature.
//! It is refreshed daily from Cloudflare, every line validated as a CIDR; a
//! list embedded in the binary covers a node that has never reached it.

use std::net::IpAddr;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Published at <https://www.cloudflare.com/ips/>.
const EMBEDDED_V4: &[&str] = &[
    "173.245.48.0/20",
    "103.21.244.0/22",
    "103.22.200.0/22",
    "103.31.4.0/22",
    "141.101.64.0/18",
    "108.162.192.0/18",
    "190.93.240.0/20",
    "188.114.96.0/20",
    "197.234.240.0/22",
    "198.41.128.0/17",
    "162.158.0.0/15",
    "104.16.0.0/13",
    "104.24.0.0/14",
    "172.64.0.0/13",
    "131.0.72.0/22",
];
const EMBEDDED_V6: &[&str] = &[
    "2400:cb00::/32",
    "2606:4700::/32",
    "2803:f800::/32",
    "2405:b500::/32",
    "2405:8100::/32",
    "2a06:98c0::/29",
    "2c0f:f248::/32",
];

const URL_V4: &str = "https://www.cloudflare.com/ips-v4";
const URL_V6: &str = "https://www.cloudflare.com/ips-v6";

/// The ranges in use, persisted in `<state>/cloudflare.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ranges {
    pub cidrs: Vec<String>,
    /// Unix time of the last successful refresh; 0 — the embedded list.
    pub updated_at: i64,
}

impl Ranges {
    pub fn embedded() -> Self {
        Self {
            cidrs: EMBEDDED_V4
                .iter()
                .chain(EMBEDDED_V6)
                .map(|s| s.to_string())
                .collect(),
            updated_at: 0,
        }
    }

    /// The stored list, or the embedded one when there is none (or it is
    /// unreadable — never a reason to trust nothing and break real IPs).
    pub fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Ranges>(&bytes).ok())
            .filter(|r| !r.cidrs.is_empty() && r.cidrs.iter().all(|c| valid_cidr(c)))
            .unwrap_or_else(Self::embedded)
    }

    /// The `set_real_ip_from` block, shared by the http-level file and the
    /// per-site include.
    pub fn directives(&self) -> String {
        let mut out = String::from("# Cloudflare edge ranges — managed by asc-daemon.\n");
        for cidr in &self.cidrs {
            out.push_str(&format!("set_real_ip_from {cidr};\n"));
        }
        out.push_str("real_ip_header CF-Connecting-IP;\nreal_ip_recursive on;\n");
        out
    }
}

/// `a.b.c.d/n` or `v6::/n` with a prefix length the family allows.
pub fn valid_cidr(value: &str) -> bool {
    let Some((addr, prefix)) = value.split_once('/') else {
        return false;
    };
    let Ok(addr) = addr.parse::<IpAddr>() else {
        return false;
    };
    let Ok(prefix) = prefix.parse::<u8>() else {
        return false;
    };
    match addr {
        IpAddr::V4(_) => prefix <= 32,
        IpAddr::V6(_) => prefix <= 128,
    }
}

/// Parses one of Cloudflare's plain-text lists. Any invalid line rejects the
/// whole response: a captive portal or an error page must not become the
/// list of trusted proxies.
pub fn parse_list(body: &str) -> Result<Vec<String>> {
    let cidrs: Vec<String> = body
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    if cidrs.is_empty() {
        bail!("empty range list");
    }
    if let Some(bad) = cidrs.iter().find(|c| !valid_cidr(c)) {
        bail!("not a CIDR: {bad:?}");
    }
    Ok(cidrs)
}

fn fetch(url: &str) -> Result<Vec<String>> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(20)))
        .build()
        .new_agent();
    let body = agent
        .get(url)
        .call()
        .with_context(|| format!("cannot fetch {url}"))?
        .body_mut()
        .with_config()
        .limit(64 * 1024)
        .read_to_string()
        .with_context(|| format!("cannot read {url}"))?;
    parse_list(&body).with_context(|| format!("unexpected response from {url}"))
}

/// Downloads both lists. Blocking — call from a worker thread.
pub fn download(now: i64) -> Result<Ranges> {
    let mut cidrs = fetch(URL_V4)?;
    cidrs.extend(fetch(URL_V6)?);
    Ok(Ranges {
        cidrs,
        updated_at: now,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_list_is_valid() {
        assert!(Ranges::embedded().cidrs.iter().all(|c| valid_cidr(c)));
    }

    #[test]
    fn parser_rejects_html() {
        assert!(parse_list("<html>blocked</html>").is_err());
        assert!(parse_list("").is_err());
        assert_eq!(
            parse_list("173.245.48.0/20\n\n2400:cb00::/32\n").unwrap(),
            vec!["173.245.48.0/20", "2400:cb00::/32"]
        );
    }

    #[test]
    fn cidr_prefixes_are_bounded() {
        assert!(!valid_cidr("10.0.0.0/33"));
        assert!(!valid_cidr("10.0.0.0"));
        assert!(valid_cidr("::/0"));
    }

    #[test]
    fn directives_trust_only_listed_ranges() {
        let text = Ranges::embedded().directives();
        assert!(text.contains("set_real_ip_from 173.245.48.0/20;"));
        assert!(text.contains("real_ip_header CF-Connecting-IP;"));
    }
}
