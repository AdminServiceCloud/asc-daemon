//! Active health checks of upstream servers (DMN-126).
//!
//! Open-source nginx only has passive checks (`max_fails`): a dead server is
//! noticed by failing real requests. For a load balancer that is not good
//! enough, so the daemon probes every server of a site with a health check
//! itself — a TCP connect or an HTTP request — and marks one that fails
//! `fails` times in a row `down` in the upstream until it passes `passes`
//! times in a row. Probes go to the server directly, not through nginx, so a
//! server taken out of rotation keeps being probed and comes back on its own.
//!
//! When every server of a site is failing, none is marked down: an upstream
//! with nothing left is worse than letting nginx try.
//!
//! State lives in memory only. After a restart every server starts healthy
//! and unchecked; the first probes settle it within one interval.

use std::collections::{HashMap, HashSet};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::model::{HealthCheck, HealthKind, UpstreamHealth};
use super::unix_now;

/// (site id, upstream address)
pub type Key = (String, String);

#[derive(Debug, Clone)]
struct Entry {
    health: UpstreamHealth,
    fails_in_row: u32,
    passes_in_row: u32,
    next_due: Instant,
    /// The check the entry was last probed with — a changed check restarts
    /// the counters.
    check: HealthCheck,
}

/// One server to probe, as the background pass finds it.
#[derive(Debug, Clone)]
pub struct Target {
    pub site: String,
    pub address: String,
    pub check: HealthCheck,
    pub tls: bool,
}

/// Live health of every probed server.
#[derive(Default)]
pub struct Registry {
    entries: Mutex<HashMap<Key, Entry>>,
}

impl Registry {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Key, Entry>> {
        self.entries.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Whether a server is currently out of rotation.
    pub fn is_down(&self, site: &str, address: &str) -> bool {
        self.lock()
            .get(&(site.to_string(), address.to_string()))
            .is_some_and(|e| e.health.checked && !e.health.healthy)
    }

    /// Health of a site's servers in upstream order; empty entries for
    /// servers never probed.
    pub fn snapshot(&self, site: &str, addresses: &[String]) -> Vec<UpstreamHealth> {
        let entries = self.lock();
        addresses
            .iter()
            .map(|address| {
                entries
                    .get(&(site.to_string(), address.clone()))
                    .map(|e| e.health.clone())
                    .unwrap_or_else(|| UpstreamHealth {
                        address: address.clone(),
                        healthy: true,
                        ..Default::default()
                    })
            })
            .collect()
    }

    /// Drops entries no site probes any more.
    pub fn retain(&self, keep: &HashSet<Key>) {
        self.lock().retain(|key, _| keep.contains(key));
    }

    /// Targets whose next probe is due, registering new ones.
    pub fn due(&self, targets: &[Target], now: Instant) -> Vec<Target> {
        let mut entries = self.lock();
        let mut due = Vec::new();
        for target in targets {
            let key = (target.site.clone(), target.address.clone());
            let entry = entries.entry(key).or_insert_with(|| Entry {
                health: UpstreamHealth {
                    address: target.address.clone(),
                    healthy: true,
                    ..Default::default()
                },
                fails_in_row: 0,
                passes_in_row: 0,
                next_due: now,
                check: target.check.clone(),
            });
            if entry.check != target.check {
                entry.check = target.check.clone();
                entry.fails_in_row = 0;
                entry.passes_in_row = 0;
                entry.next_due = now;
            }
            if entry.next_due <= now {
                // Not probed again until this one reports back.
                entry.next_due = now + Duration::from_secs(3600);
                due.push(target.clone());
            }
        }
        due
    }

    /// Records a probe result; true when the server flipped in or out of
    /// rotation.
    pub fn record(&self, target: &Target, result: Result<u32, String>, now: Instant) -> bool {
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(&(target.site.clone(), target.address.clone())) else {
            return false;
        };
        let check = &target.check;
        entry.next_due = now + Duration::from_secs(u64::from(check.interval()));
        entry.health.checked_at = unix_now();
        let was_down = entry.health.checked && !entry.health.healthy;
        match result {
            Ok(latency) => {
                entry.fails_in_row = 0;
                entry.passes_in_row = entry.passes_in_row.saturating_add(1);
                entry.health.latency_ms = latency;
                entry.health.last_error.clear();
                if !entry.health.checked || entry.passes_in_row >= check.passes() {
                    entry.health.healthy = true;
                }
            }
            Err(error) => {
                entry.passes_in_row = 0;
                entry.fails_in_row = entry.fails_in_row.saturating_add(1);
                entry.health.last_error = error;
                if entry.fails_in_row >= check.fails() {
                    entry.health.healthy = false;
                }
            }
        }
        entry.health.checked = true;
        let is_down = !entry.health.healthy;
        was_down != is_down
    }
}

fn resolve(address: &str) -> Result<Vec<SocketAddr>, String> {
    address
        .to_socket_addrs()
        .map(|a| a.collect::<Vec<_>>())
        .map_err(|e| format!("cannot resolve {address}: {e}"))
        .and_then(|list| {
            if list.is_empty() {
                Err(format!("{address} resolves to nothing"))
            } else {
                Ok(list)
            }
        })
}

/// A TCP connect, returning the latency in milliseconds. Blocking.
pub fn probe_tcp(address: &str, timeout: Duration) -> Result<u32, String> {
    let started = Instant::now();
    let mut last = String::new();
    for socket in resolve(address)? {
        match TcpStream::connect_timeout(&socket, timeout) {
            Ok(_) => return Ok(started.elapsed().as_millis().min(u128::from(u32::MAX)) as u32),
            Err(e) => last = format!("connect {socket}: {e}"),
        }
    }
    Err(last)
}

/// An HTTP(S) GET of `path`; a status outside the expected one (or outside
/// 2xx/3xx when none is set) fails. Certificates are not verified: upstream
/// servers are addressed by IP and commonly serve self-signed ones. Blocking.
pub fn probe_http(
    address: &str,
    check: &HealthCheck,
    tls: bool,
    timeout: Duration,
) -> Result<u32, String> {
    let scheme = if tls { "https" } else { "http" };
    let url = format!(
        "{scheme}://{address}{}",
        if check.path.is_empty() {
            "/"
        } else {
            &check.path
        }
    );
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .max_redirects(0)
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .disable_verification(true)
                .build(),
        )
        .build()
        .into();
    let started = Instant::now();
    let response = agent
        .get(&url)
        .header("User-Agent", "asc-daemon-healthcheck")
        .call()
        .map_err(|e| format!("GET {url}: {e}"))?;
    let status = response.status().as_u16();
    let ok = if check.expected_status == 0 {
        (200..400).contains(&status)
    } else {
        status == check.expected_status
    };
    if ok {
        Ok(started.elapsed().as_millis().min(u128::from(u32::MAX)) as u32)
    } else {
        Err(format!("GET {url} answered {status}"))
    }
}

/// Runs one probe of a target. Blocking.
pub fn probe(target: &Target) -> Result<u32, String> {
    let timeout = Duration::from_secs(u64::from(target.check.timeout()));
    match target.check.kind {
        HealthKind::Off => Ok(0),
        HealthKind::Tcp => probe_tcp(&target.address, timeout),
        HealthKind::Http => probe_http(&target.address, &target.check, target.tls, timeout),
    }
}

/// Which servers of a site to take out of rotation: the unhealthy ones,
/// unless that would be all of them.
pub fn down_mask(registry: &Registry, site: &str, addresses: &[String]) -> Vec<bool> {
    let mask: Vec<bool> = addresses
        .iter()
        .map(|a| registry.is_down(site, a))
        .collect();
    if !mask.is_empty() && mask.iter().all(|down| *down) {
        vec![false; mask.len()]
    } else {
        mask
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(check: HealthCheck) -> Target {
        Target {
            site: "s".into(),
            address: "127.0.0.1:1".into(),
            check,
            tls: false,
        }
    }

    #[test]
    fn fails_and_passes_in_a_row_flip_state() {
        let registry = Registry::default();
        let t = target(HealthCheck {
            kind: HealthKind::Tcp,
            fails: 2,
            passes: 2,
            ..Default::default()
        });
        let now = Instant::now();
        assert_eq!(registry.due(std::slice::from_ref(&t), now).len(), 1);
        assert!(
            !registry.record(&t, Err("x".into()), now),
            "one failure is not enough"
        );
        assert!(!registry.is_down("s", "127.0.0.1:1"));
        assert!(
            registry.record(&t, Err("x".into()), now),
            "second failure flips"
        );
        assert!(registry.is_down("s", "127.0.0.1:1"));
        assert!(!registry.record(&t, Ok(1), now));
        assert!(
            registry.record(&t, Ok(1), now),
            "second pass brings it back"
        );
        assert!(!registry.is_down("s", "127.0.0.1:1"));
    }

    #[test]
    fn nothing_is_due_until_the_interval_passes() {
        let registry = Registry::default();
        let t = target(HealthCheck {
            kind: HealthKind::Tcp,
            interval_secs: 10,
            ..Default::default()
        });
        let now = Instant::now();
        assert_eq!(registry.due(std::slice::from_ref(&t), now).len(), 1);
        assert!(
            registry.due(std::slice::from_ref(&t), now).is_empty(),
            "in flight"
        );
        registry.record(&t, Ok(1), now);
        assert!(
            registry
                .due(std::slice::from_ref(&t), now + Duration::from_secs(5))
                .is_empty()
        );
        assert_eq!(
            registry
                .due(std::slice::from_ref(&t), now + Duration::from_secs(10))
                .len(),
            1
        );
    }

    #[test]
    fn all_down_takes_none_out() {
        let registry = Registry::default();
        let check = HealthCheck {
            kind: HealthKind::Tcp,
            fails: 1,
            ..Default::default()
        };
        let a = Target {
            address: "a:1".into(),
            ..target(check.clone())
        };
        let b = Target {
            address: "b:1".into(),
            ..target(check)
        };
        let now = Instant::now();
        registry.due(&[a.clone(), b.clone()], now);
        registry.record(&a, Err("x".into()), now);
        let addresses = vec!["a:1".to_string(), "b:1".to_string()];
        assert_eq!(down_mask(&registry, "s", &addresses), vec![true, false]);
        registry.record(&b, Err("x".into()), now);
        assert_eq!(down_mask(&registry, "s", &addresses), vec![false, false]);
    }

    #[test]
    fn tcp_probe_reports_refused_ports() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let open = listener.local_addr().unwrap().to_string();
        assert!(probe_tcp(&open, Duration::from_secs(1)).is_ok());
        drop(listener);
        assert!(probe_tcp(&open, Duration::from_secs(1)).is_err());
    }
}
