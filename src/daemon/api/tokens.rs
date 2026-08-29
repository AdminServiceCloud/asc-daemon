//! Primary and access tokens (DMN-065, DMN-066 — see docs/security-tokens.md).
//!
//! The daemon's TCP API is guarded by two kinds of bearer token. The
//! **primary** is the long-lived one in `api.token`, the credential the
//! platform stores at enrollment; from here on it behaves like a refresh
//! token — its job is to mint other tokens and to be rotated, not to sign
//! everyday traffic. An **access** token is short-lived, lives only in this
//! store and is minted by presenting the primary. It carries the same
//! authority for everything except token management (see [`require_primary`]).
//!
//! Access tokens are keyed by their SHA-256 digest rather than by the token
//! itself: this map lives for minutes, unlike the 30-second console tokens,
//! and a core dump or a stray `Debug` print must not yield working
//! credentials. The digest lookup is not constant-time, but the key is 256
//! bits of CSPRNG entropy — the same trade-off [`super::console`] already
//! makes.

use std::collections::HashMap;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::daemon::apps::UserContext;

use super::console::{constant_time_eq, random_hex};

/// How long a minted access token stays valid unless the caller asks for less.
pub const ACCESS_TTL: Duration = Duration::from_secs(600);

/// Upper bound on a requested access-token TTL. Beyond this the point of a
/// short-lived credential is gone; the caller should rotate instead.
pub const ACCESS_TTL_MAX: Duration = Duration::from_secs(3600);

/// How many access tokens may be alive at once. A platform that leaks
/// refreshes must not be able to grow this map without bound; past the cap
/// the oldest entry is evicted.
pub const MAX_LIVE_ACCESS: usize = 64;

/// Default grace window: how long the previous primary keeps working after a
/// rotation, giving the platform time to persist the new one.
pub const ROTATION_GRACE: Duration = Duration::from_secs(300);

/// Longest grace window a caller may ask for.
pub const ROTATION_GRACE_MAX: Duration = Duration::from_secs(3600);

/// How the presented credential was recognised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// The current primary, or the previous one inside a rotation's grace
    /// window (`grace` on [`Resolved`] tells the two apart).
    Primary,
    /// A short-lived token minted from the primary.
    Access,
    /// The unix socket: identity came from the kernel, not from a token.
    LocalPeer,
}

/// The outcome of classifying a presented bearer token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolved {
    pub kind: TokenKind,
    /// True when the request authenticated with the *previous* primary during
    /// a rotation. Such a caller may act, but may not confirm the rotation:
    /// a platform that stored nothing must not be able to report success.
    pub grace: bool,
    /// Unix seconds when the presented token expires; set only for an access
    /// token. Captured here so handlers never have to carry the secret
    /// itself around just to report its lifetime.
    pub expires_at: Option<i64>,
}

impl Resolved {
    /// The unix socket's classification: no token, identity from the kernel.
    pub fn local_peer() -> Self {
        Self::of(TokenKind::LocalPeer)
    }

    fn of(kind: TokenKind) -> Self {
        Self {
            kind,
            grace: false,
            expires_at: None,
        }
    }
}

/// Refusal to run a token-management operation with something other than the
/// primary. Rendered as `403` by REST and `PERMISSION_DENIED` by gRPC.
#[derive(Debug)]
pub struct TokenDenied {
    pub reason: &'static str,
}

impl std::fmt::Display for TokenDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason)
    }
}

impl std::error::Error for TokenDenied {}

/// Managing tokens requires the primary token, or root on the unix socket —
/// that is what makes `sudo asc api token rotate` work while a regular user
/// on the same socket is refused. Anything else fails closed, including a
/// missing classification (which would mean a middleware bug).
pub fn require_primary(resolved: Option<Resolved>, ctx: &UserContext) -> Result<(), TokenDenied> {
    match resolved.map(|r| r.kind) {
        Some(TokenKind::Primary) => Ok(()),
        Some(TokenKind::LocalPeer) if ctx.is_root => Ok(()),
        Some(TokenKind::LocalPeer) => Err(TokenDenied {
            reason: "managing API tokens on the socket requires root",
        }),
        Some(TokenKind::Access) => Err(TokenDenied {
            reason: "access tokens cannot manage API tokens",
        }),
        None => Err(TokenDenied {
            reason: "the request carries no recognised API token",
        }),
    }
}

/// A rotation that is waiting to be confirmed must be confirmed with the new
/// token, never with the one it replaced.
pub fn reject_grace(resolved: Option<Resolved>) -> Result<(), TokenDenied> {
    if resolved.is_some_and(|r| r.grace) {
        return Err(TokenDenied {
            reason: "confirm the rotation with the new token, not the previous one",
        });
    }
    Ok(())
}

struct Primary {
    current: String,
    /// The token replaced by the last rotation and the moment it stops
    /// working. Memory only: a daemon restart ends the window early, which
    /// is why the grace is a cushion and not a guarantee.
    previous: Option<(String, Instant)>,
}

struct AccessEntry {
    expires: Instant,
    expires_at: i64,
    #[allow(dead_code)] // Surfaced by `asc api status`; see DMN-066.
    label: String,
}

/// What a rotation produced.
pub struct Rotation {
    pub token: String,
    pub rotated_at: i64,
    /// Unix seconds until which the previous primary still works; `None`
    /// when the caller asked for no grace window.
    pub grace_until: Option<i64>,
    /// How many access tokens the rotation revoked on the way.
    pub revoked: usize,
}

/// What `GET /v1/token` reports. Never carries token material.
pub struct Status {
    pub kind: TokenKind,
    /// Unix seconds; set only for an access token.
    pub expires_at: Option<i64>,
    pub access_tokens_live: usize,
    /// First 16 hex characters of the primary's SHA-256: enough for the
    /// platform to answer "is the token I hold still the current one"
    /// without either side transmitting it.
    pub primary_digest: String,
    pub rotation_pending: bool,
    pub grace_until: Option<i64>,
    pub ttl_default_secs: u64,
}

/// The daemon's live token state, shared behind [`super::ApiState`].
pub struct TokenStore {
    primary: RwLock<Primary>,
    access: Mutex<HashMap<[u8; 32], AccessEntry>>,
}

impl TokenStore {
    pub fn new(primary: String) -> Self {
        Self {
            primary: RwLock::new(Primary {
                current: primary,
                previous: None,
            }),
            access: Mutex::new(HashMap::new()),
        }
    }

    /// The current primary. Only ever leaves the daemon through
    /// `asc api token show` (root, unix socket) and a rotation's response.
    pub fn primary(&self) -> String {
        self.read_primary().current.clone()
    }

    /// Classify a presented bearer token. Order matters: the current primary
    /// first, then the grace token, then the access map.
    pub fn resolve(&self, presented: &str) -> Option<Resolved> {
        {
            let primary = self.read_primary();
            if constant_time_eq(presented, &primary.current) {
                return Some(Resolved::of(TokenKind::Primary));
            }
            if let Some((previous, deadline)) = &primary.previous
                && Instant::now() < *deadline
                && constant_time_eq(presented, previous)
            {
                return Some(Resolved {
                    kind: TokenKind::Primary,
                    grace: true,
                    expires_at: None,
                });
            }
        }
        let mut access = self.lock_access();
        Self::sweep(&mut access);
        access.get(&digest(presented)).map(|entry| Resolved {
            kind: TokenKind::Access,
            grace: false,
            expires_at: Some(entry.expires_at),
        })
    }

    /// Mint an access token. Returns `(token, expires_at)` with `expires_at`
    /// in Unix seconds, the same shape console tokens use.
    pub fn issue_access(&self, ttl: Option<Duration>, label: &str) -> (String, i64) {
        let ttl = ttl
            .unwrap_or(ACCESS_TTL)
            .clamp(Duration::from_secs(1), ACCESS_TTL_MAX);
        let token = random_hex(32);
        let expires = Instant::now() + ttl;
        let expires_at = unix_seconds(SystemTime::now() + ttl);

        let mut access = self.lock_access();
        Self::sweep(&mut access);
        // The cap is a memory bound, not a policy: drop whatever expires
        // soonest so a live token is never evicted while a staler one stays.
        while access.len() >= MAX_LIVE_ACCESS {
            let Some(oldest) = access
                .iter()
                .min_by_key(|(_, entry)| entry.expires)
                .map(|(key, _)| *key)
            else {
                break;
            };
            access.remove(&oldest);
        }
        access.insert(
            digest(&token),
            AccessEntry {
                expires,
                expires_at,
                label: label.to_string(),
            },
        );
        (token, expires_at)
    }

    /// Kill every live access token, returning how many were dropped. Also
    /// the first step of a rotation, so "revoke everything" lives in exactly
    /// one place.
    pub fn revoke_all_access(&self) -> usize {
        let mut access = self.lock_access();
        Self::sweep(&mut access);
        let count = access.len();
        access.clear();
        count
    }

    /// Replace the primary. The previous one keeps working for `grace`,
    /// giving the caller time to persist the new value; `Duration::ZERO`
    /// switches over immediately.
    ///
    /// `persist` writes the new token to disk and runs **before** the swap:
    /// a store that moved on while `api.token` still held the old value would
    /// hand the platform a token the next daemon start no longer knows.
    ///
    /// Access tokens are revoked first: a rotation may well be the response
    /// to a compromise, and a token minted under the old primary must not
    /// outlive it — not even for the instant between two locks. Revoking is
    /// never the dangerous half, so it is also safe to have done when
    /// `persist` fails and the primary stays put.
    pub fn rotate(
        &self,
        grace: Duration,
        persist: impl FnOnce(&str) -> anyhow::Result<()>,
    ) -> anyhow::Result<Rotation> {
        let revoked = self.revoke_all_access();
        let grace = grace.min(ROTATION_GRACE_MAX);
        let token = random_hex(32);
        persist(&token)?;
        let now = SystemTime::now();

        let mut primary = self.write_primary();
        let replaced = std::mem::replace(&mut primary.current, token.clone());
        primary.previous = (!grace.is_zero()).then(|| (replaced, Instant::now() + grace));
        drop(primary);

        Ok(Rotation {
            token,
            rotated_at: unix_seconds(now),
            grace_until: (!grace.is_zero()).then(|| unix_seconds(now + grace)),
            revoked,
        })
    }

    /// End the grace window early: the caller has persisted the new primary.
    pub fn commit_rotation(&self) {
        self.write_primary().previous = None;
    }

    /// Status for `GET /v1/token`, from the point of view of `resolved`.
    pub fn status(&self, resolved: Resolved) -> Status {
        let (primary_digest, grace_until) = {
            let primary = self.read_primary();
            let digest = hex(&Sha256::digest(primary.current.as_bytes()))[..16].to_string();
            let grace = primary
                .previous
                .as_ref()
                .map(|(_, deadline)| unix_seconds(SystemTime::now() + remaining(*deadline)));
            (digest, grace)
        };

        let mut access = self.lock_access();
        Self::sweep(&mut access);
        let live = access.len();
        drop(access);

        Status {
            kind: resolved.kind,
            expires_at: resolved.expires_at,
            access_tokens_live: live,
            primary_digest,
            rotation_pending: grace_until.is_some(),
            grace_until,
            ttl_default_secs: ACCESS_TTL.as_secs(),
        }
    }

    fn read_primary(&self) -> std::sync::RwLockReadGuard<'_, Primary> {
        self.primary.read().expect("primary token lock poisoned")
    }

    fn write_primary(&self) -> std::sync::RwLockWriteGuard<'_, Primary> {
        self.primary.write().expect("primary token lock poisoned")
    }

    fn lock_access(&self) -> std::sync::MutexGuard<'_, HashMap<[u8; 32], AccessEntry>> {
        self.access.lock().expect("access token lock poisoned")
    }

    /// Drop expired entries. Lazy, on every operation — the same approach
    /// [`super::console::ConsoleTokens`] takes, and it keeps the store free
    /// of a background task.
    fn sweep(access: &mut HashMap<[u8; 32], AccessEntry>) {
        let now = Instant::now();
        access.retain(|_, entry| entry.expires > now);
    }
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unix_seconds(at: SystemTime) -> i64 {
    at.duration_since(UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs() as i64
}

fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> UserContext {
        UserContext {
            uid: 0,
            name: "root".into(),
            is_root: true,
        }
    }

    fn user() -> UserContext {
        UserContext {
            uid: 1000,
            name: "user".into(),
            is_root: false,
        }
    }

    #[test]
    fn primary_resolves_and_unknown_tokens_do_not() {
        let store = TokenStore::new("primary".into());
        assert_eq!(
            store.resolve("primary"),
            Some(Resolved::of(TokenKind::Primary))
        );
        assert_eq!(store.resolve("nope"), None);
    }

    #[test]
    fn access_tokens_are_multi_use_until_they_expire() {
        let store = TokenStore::new("primary".into());
        let (token, expires_at) = store.issue_access(None, "test");
        assert_eq!(token.len(), 64);
        assert!(expires_at > 0);
        // Unlike console tokens, resolving does not consume.
        let first = store.resolve(&token).expect("first use works");
        assert_eq!(first.kind, TokenKind::Access);
        assert_eq!(first.expires_at, Some(expires_at));
        assert_eq!(store.resolve(&token), Some(first));
    }

    /// A requested TTL is floored at one second, so this is the shortest
    /// lifetime a caller can actually get.
    #[test]
    fn expired_access_tokens_stop_resolving() {
        let store = TokenStore::new("primary".into());
        let (token, _) = store.issue_access(Some(Duration::from_millis(1)), "short");
        assert!(store.resolve(&token).is_some());
        std::thread::sleep(Duration::from_millis(1_100));
        assert_eq!(store.resolve(&token), None);
    }

    #[test]
    fn a_requested_ttl_is_clamped_to_the_maximum() {
        let store = TokenStore::new("primary".into());
        let before = unix_seconds(SystemTime::now());
        let (_, expires_at) = store.issue_access(Some(Duration::from_secs(86_400)), "greedy");
        assert!(expires_at - before <= ACCESS_TTL_MAX.as_secs() as i64 + 1);
    }

    #[test]
    fn the_live_cap_evicts_instead_of_growing() {
        let store = TokenStore::new("primary".into());
        for _ in 0..MAX_LIVE_ACCESS + 10 {
            store.issue_access(None, "flood");
        }
        assert_eq!(store.revoke_all_access(), MAX_LIVE_ACCESS);
    }

    #[test]
    fn revoking_kills_every_access_token_and_reports_the_count() {
        let store = TokenStore::new("primary".into());
        let (first, _) = store.issue_access(None, "a");
        let (second, _) = store.issue_access(None, "b");
        assert_eq!(store.revoke_all_access(), 2);
        assert_eq!(store.resolve(&first), None);
        assert_eq!(store.resolve(&second), None);
        // The primary is untouched by a revocation.
        assert_eq!(
            store.resolve("primary"),
            Some(Resolved::of(TokenKind::Primary))
        );
    }

    #[test]
    fn rotation_revokes_access_tokens_and_swaps_the_primary() {
        let store = TokenStore::new("primary".into());
        let (access, _) = store.issue_access(None, "doomed");
        let rotation = store.rotate(ROTATION_GRACE, |_| Ok(())).unwrap();

        assert_eq!(rotation.revoked, 1);
        assert_eq!(store.resolve(&access), None);
        assert_eq!(store.primary(), rotation.token);
        assert_eq!(
            store.resolve(&rotation.token),
            Some(Resolved::of(TokenKind::Primary))
        );
    }

    #[test]
    fn the_previous_primary_works_inside_the_grace_window() {
        let store = TokenStore::new("primary".into());
        let rotation = store.rotate(ROTATION_GRACE, |_| Ok(())).unwrap();
        assert!(rotation.grace_until.is_some());
        assert_eq!(
            store.resolve("primary"),
            Some(Resolved {
                kind: TokenKind::Primary,
                grace: true,
                expires_at: None,
            })
        );
    }

    #[test]
    fn the_previous_primary_dies_when_the_window_closes() {
        let store = TokenStore::new("primary".into());
        store.rotate(Duration::from_millis(1), |_| Ok(())).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(store.resolve("primary"), None);
    }

    #[test]
    fn a_zero_grace_rotation_drops_the_previous_primary_at_once() {
        let store = TokenStore::new("primary".into());
        let rotation = store.rotate(Duration::ZERO, |_| Ok(())).unwrap();
        assert!(rotation.grace_until.is_none());
        assert_eq!(store.resolve("primary"), None);
    }

    #[test]
    fn committing_ends_the_grace_window() {
        let store = TokenStore::new("primary".into());
        store.rotate(ROTATION_GRACE, |_| Ok(())).unwrap();
        store.commit_rotation();
        assert_eq!(store.resolve("primary"), None);
    }

    #[test]
    fn only_the_primary_may_manage_tokens() {
        assert!(require_primary(Some(Resolved::of(TokenKind::Primary)), &root()).is_ok());
        assert!(require_primary(Some(Resolved::of(TokenKind::Access)), &root()).is_err());
        assert!(require_primary(None, &root()).is_err());
    }

    #[test]
    fn on_the_socket_only_root_may_manage_tokens() {
        assert!(require_primary(Some(Resolved::of(TokenKind::LocalPeer)), &root()).is_ok());
        assert!(require_primary(Some(Resolved::of(TokenKind::LocalPeer)), &user()).is_err());
    }

    #[test]
    fn a_rotation_cannot_be_confirmed_with_the_token_it_replaced() {
        assert!(reject_grace(Some(Resolved::of(TokenKind::Primary))).is_ok());
        assert!(
            reject_grace(Some(Resolved {
                kind: TokenKind::Primary,
                grace: true,
                expires_at: None,
            }))
            .is_err()
        );
    }

    #[test]
    fn status_never_carries_the_primary() {
        let store = TokenStore::new("primary".into());
        let (access, expires_at) = store.issue_access(None, "shown");
        let resolved = store.resolve(&access).expect("just issued");
        let status = store.status(resolved);

        assert_eq!(status.expires_at, Some(expires_at));
        assert_eq!(status.access_tokens_live, 1);
        assert_eq!(status.primary_digest.len(), 16);
        assert!(!status.primary_digest.contains("primary"));
        assert!(!status.rotation_pending);
    }
}
