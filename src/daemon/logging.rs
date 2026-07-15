//! Logging setup via `tracing`.
//!
//! The level comes from config (`log.level`, toggled by `asc config debug`),
//! the `RUST_LOG` environment variable takes precedence. Log output is
//! English-only (not translated) and goes to stderr, so it never mixes into
//! stdout that scripts might parse (`asc status | ...`).

use tracing_subscriber::EnvFilter;

/// Initialize the global tracing subscriber. Call once, before any log output.
/// `init()` is called from `run()` for every command, not just `serve` —
/// one-shot commands like `asc install` run daemon logic in-process too.
pub fn init(default_level: &str) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
