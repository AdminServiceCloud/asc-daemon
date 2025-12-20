//! asc-daemon library crate, shared by the `asc` and `asc-updater` binaries.

// The daemon ships for Linux only (macOS is on the roadmap). There are no
// Windows code paths on purpose — develop with WSL or a Linux target.
#[cfg(not(unix))]
compile_error!(
    "asc-daemon targets Linux (and macOS later); build with --target x86_64-unknown-linux-gnu"
);

pub mod daemon;

/// Version of the daemon, taken from Cargo.toml at build time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// AdminService.Cloud ASCII banner, shown by `asc status` (on a terminal)
/// and when the daemon's API service starts.
pub const BANNER: &str = r#"  /$$$$$$        /$$               /$$            /$$$$$$                                 /$$                          /$$$$$$  /$$                           /$$
 /$$__  $$      | $$              |__/           /$$__  $$                               |__/                         /$$__  $$| $$                          | $$
| $$  \ $$  /$$$$$$$ /$$$$$$/$$$$  /$$ /$$$$$$$ | $$  \__/  /$$$$$$   /$$$$$$  /$$    /$$ /$$  /$$$$$$$  /$$$$$$     | $$  \__/| $$  /$$$$$$  /$$   /$$  /$$$$$$$
| $$$$$$$$ /$$__  $$| $$_  $$_  $$| $$| $$__  $$|  $$$$$$  /$$__  $$ /$$__  $$|  $$  /$$/| $$ /$$_____/ /$$__  $$    | $$      | $$ /$$__  $$| $$  | $$ /$$__  $$
| $$__  $$| $$  | $$| $$ \ $$ \ $$| $$| $$  \ $$ \____  $$| $$$$$$$$| $$  \__/ \  $$/$$/ | $$| $$      | $$$$$$$$    | $$      | $$| $$  \ $$| $$  | $$| $$  | $$
| $$  | $$| $$  | $$| $$ | $$ | $$| $$| $$  | $$ /$$  \ $$| $$_____/| $$        \  $$$/  | $$| $$      | $$_____/    | $$    $$| $$| $$  | $$| $$  | $$| $$  | $$
| $$  | $$|  $$$$$$$| $$ | $$ | $$| $$| $$  | $$|  $$$$$$/|  $$$$$$$| $$         \  $/   | $$|  $$$$$$$|  $$$$$$$ /$$|  $$$$$$/| $$|  $$$$$$/|  $$$$$$/|  $$$$$$$
|__/  |__/ \_______/|__/ |__/ |__/|__/|__/  |__/ \______/  \_______/|__/          \_/    |__/ \_______/ \_______/|__/ \______/ |__/ \______/  \______/  \_______/"#;
