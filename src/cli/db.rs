//! `asc db`: the daemon's local database — the index of installed apps
//! that mirrors every app's `.asc/` (see docs/english/app-management.md).
//!
//! Through the daemon when it runs (the database is its file, root only);
//! straight on the file otherwise, which needs the same rights the daemon
//! has over the data directory.

use clap::Subcommand;

use asc_daemon::daemon::apps::AppManager;
use asc_daemon::daemon::apps::index::{self, DbError, IndexStatus, RebuildReport, VerifyReport};
use asc_daemon::daemon::config::Config;
use asc_daemon::daemon::i18n::{Msg, t, tf, tf2};
use asc_daemon::daemon::monitor::human_bytes;

use super::daemon_backend;

#[derive(Subcommand)]
pub enum DbAction {
    /// Show where the database is, how many apps and files it holds, its
    /// size and when it was last rebuilt and verified
    Status,
    /// Compare the database with the app files (`stat` only) and fix what
    /// differs
    Verify,
    /// Rebuild the database from the app files
    Rebuild,
}

pub fn run(action: DbAction, config: &Config) -> anyhow::Result<()> {
    if let Some(daemon) = daemon_backend(config)? {
        match action {
            DbAction::Status => print_status(&daemon.db_status()?),
            DbAction::Verify => print_verify(&daemon.db_verify()?),
            DbAction::Rebuild => print_rebuild(&daemon.db_rebuild()?),
        }
        return Ok(());
    }
    // In-process: the database of this config's apps directory — the
    // caller's own tree for a regular user, the system one for root.
    let manager = AppManager::new(config);
    let Some(index) = manager.store().index() else {
        anyhow::bail!(DbError::Disabled);
    };
    match action {
        DbAction::Status => print_status(&index.status()?),
        DbAction::Verify => print_verify(&index.verify()?),
        DbAction::Rebuild => print_rebuild(&index.rebuild()?),
    }
    Ok(())
}

/// "3m 12s ago"-style age of a unix-ms timestamp; "never" without one.
fn ago(at: Option<i64>) -> String {
    let Some(at) = at else {
        return t(Msg::DbNever).to_string();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default();
    let secs = (now - at).max(0) / 1000;
    let span = match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m {}s", secs / 60, secs % 60),
        3600..86400 => format!("{}h {}m", secs / 3600, secs % 3600 / 60),
        _ => format!("{}d {}h", secs / 86400, secs % 86400 / 3600),
    };
    tf(Msg::DbAgo, span)
}

fn print_status(status: &IndexStatus) {
    println!("{}", tf(Msg::DbStatusPath, status.path.display()));
    println!("{}", tf(Msg::DbStatusRoot, status.apps_root.display()));
    println!("{}", tf(Msg::DbStatusSchema, status.schema_version));
    println!("{}", tf2(Msg::DbStatusContents, status.apps, status.files));
    println!("{}", tf(Msg::DbStatusSize, human_bytes(status.size_bytes)));
    println!("{}", tf(Msg::DbStatusRebuilt, ago(status.last_full_scan)));
    println!("{}", tf(Msg::DbStatusVerified, ago(status.last_verify)));
    let watcher = if status.watching {
        t(Msg::StateActive)
    } else {
        t(Msg::StateInactive)
    };
    println!("{}", tf(Msg::DbStatusWatcher, watcher));
    println!(
        "{}",
        tf(
            Msg::DbStatusMirrorLimit,
            human_bytes(index::MAX_MIRRORED_BYTES)
        )
    );
}

fn print_verify(report: &VerifyReport) {
    println!(
        "{}",
        t(Msg::DbVerified)
            .replacen("{}", &report.checked.to_string(), 1)
            .replacen("{}", &report.added.to_string(), 1)
            .replacen("{}", &report.updated.to_string(), 1)
            .replacen("{}", &report.removed.to_string(), 1)
            .replacen("{}", &report.millis.to_string(), 1)
    );
}

fn print_rebuild(report: &RebuildReport) {
    println!("{}", tf2(Msg::DbRebuilt, report.apps, report.millis));
    if report.skipped > 0 {
        println!("{}", tf(Msg::DbRebuildSkipped, report.skipped));
    }
}
