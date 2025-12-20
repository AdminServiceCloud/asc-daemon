//! Logging setup via `tracing`.
//!
//! The level comes from config (`log.level`), the `RUST_LOG` environment
//! variable takes precedence. Log output is English-only (not translated).

use tracing_subscriber::EnvFilter;

/// Initialize the global tracing subscriber. Call once, before any log output.
pub fn init(default_level: &str) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
