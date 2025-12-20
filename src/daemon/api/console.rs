//! Temporary console tokens (see docs/console.md).
//!
//! A WebSocket console session (DMN-007) can only be opened with a one-time
//! token issued through the API: short TTL, bound to one app and one session
//! type. The platform requests a token after its own permission check; in
//! standalone mode the CLI will do the same.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long an issued token stays valid.
pub const TOKEN_TTL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionType {
    Logs,
    Attach,
}

#[derive(Debug, Clone)]
pub struct ConsoleGrant {
    pub app_id: String,
    pub session: SessionType,
}

struct Entry {
    grant: ConsoleGrant,
    expires: Instant,
}

/// In-memory store of issued tokens. Tokens are single-use: `consume`
/// removes them; expired entries are dropped lazily on every operation.
#[derive(Default)]
pub struct ConsoleTokens {
    entries: Mutex<HashMap<String, Entry>>,
}

impl ConsoleTokens {
    /// Issue a token for one console session. Returns `(token, expires_at)`
    /// where `expires_at` is Unix seconds (for API clients).
    pub fn issue(&self, app_id: &str, session: SessionType) -> (String, i64) {
        let token = random_hex(32);
        let expires_at = (SystemTime::now() + TOKEN_TTL)
            .duration_since(UNIX_EPOCH)
            .expect("system clock before 1970")
            .as_secs() as i64;
        let mut entries = self.entries.lock().expect("console token lock poisoned");
        entries.retain(|_, e| e.expires > Instant::now());
        entries.insert(
            token.clone(),
            Entry {
                grant: ConsoleGrant {
                    app_id: app_id.to_string(),
                    session,
                },
                expires: Instant::now() + TOKEN_TTL,
            },
        );
        (token, expires_at)
    }

    /// Redeem a token: valid at most once, and only before its TTL.
    /// Used by the WebSocket console handshake (DMN-007).
    pub fn consume(&self, token: &str) -> Option<ConsoleGrant> {
        let mut entries = self.entries.lock().expect("console token lock poisoned");
        entries.retain(|_, e| e.expires > Instant::now());
        entries.remove(token).map(|e| e.grant)
    }
}

/// Cryptographically random lowercase-hex string, `bytes * 2` chars long.
/// Reads the kernel CSPRNG directly — no extra dependency.
pub fn random_hex(bytes: usize) -> String {
    use std::io::Read;
    let mut buf = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .expect("cannot read /dev/urandom");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Constant-time string equality for secrets (API/console tokens).
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_single_use() {
        let tokens = ConsoleTokens::default();
        let (token, expires_at) = tokens.issue("demo", SessionType::Logs);
        assert_eq!(token.len(), 64);
        assert!(expires_at > 0);
        let grant = tokens.consume(&token).expect("first use works");
        assert_eq!(grant.app_id, "demo");
        assert_eq!(grant.session, SessionType::Logs);
        assert!(tokens.consume(&token).is_none(), "second use must fail");
    }

    #[test]
    fn unknown_token_is_rejected() {
        let tokens = ConsoleTokens::default();
        assert!(tokens.consume("deadbeef").is_none());
    }

    #[test]
    fn tokens_are_unique() {
        assert_ne!(random_hex(32), random_hex(32));
    }

    #[test]
    fn constant_time_compare() {
        assert!(constant_time_eq("secret", "secret"));
        assert!(!constant_time_eq("secret", "secreT"));
        assert!(!constant_time_eq("secret", "longer-secret"));
        assert!(constant_time_eq("", ""));
    }
}
