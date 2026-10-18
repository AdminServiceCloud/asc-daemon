//! Schema of the index database and its built-in migrations.
//!
//! The version lives in `schema_meta.version`. An older database is
//! migrated step by step; a newer one (written by a later daemon) is not
//! something this build can trust, and the caller rebuilds it from the
//! files instead — the index is always recoverable from disk.

use rusqlite::{Connection, OptionalExtension};

/// Version this build reads and writes.
pub const VERSION: i64 = 1;

/// The whole v1 schema. `IF NOT EXISTS` keeps it safe on a half-created
/// database (a crash between two statements).
const V1: &str = "
CREATE TABLE IF NOT EXISTS schema_meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS apps (
  id              TEXT PRIMARY KEY,
  uuid            TEXT,
  owner_uid       INTEGER NOT NULL,
  custom_name     TEXT,
  custom_name_ci  TEXT,
  runtime_kind    TEXT NOT NULL,
  container_name  TEXT,
  compose_project TEXT,
  desired_state   TEXT NOT NULL,
  meta_json       TEXT NOT NULL,
  commit_sha      TEXT,
  git_stamp       TEXT NOT NULL,
  observed_state  TEXT,
  observed_at     INTEGER,
  synced_at       INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS apps_owner   ON apps(owner_uid);
CREATE INDEX IF NOT EXISTS apps_name_ci ON apps(custom_name_ci);
CREATE INDEX IF NOT EXISTS apps_project ON apps(compose_project);
CREATE TABLE IF NOT EXISTS app_files (
  app_id  TEXT NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
  name    TEXT NOT NULL,
  content BLOB,
  size    INTEGER NOT NULL,
  stamp   TEXT NOT NULL,
  PRIMARY KEY (app_id, name)
);
";

/// What [`prepare`] found.
#[derive(Debug, PartialEq, Eq)]
pub enum Prepared {
    /// The schema is current.
    Ready,
    /// The schema was just created (or migrated in a way that dropped
    /// rows): the index is empty and needs a full scan.
    Empty,
    /// Written by a newer daemon: do not touch, rebuild from scratch.
    Newer(i64),
}

/// Bring the schema to [`VERSION`].
pub fn prepare(conn: &Connection) -> rusqlite::Result<Prepared> {
    let has_meta: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_meta'",
            [],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    let version = if has_meta {
        get(conn, "version")?.and_then(|v| v.parse::<i64>().ok())
    } else {
        None
    };
    match version {
        Some(v) if v == VERSION => Ok(Prepared::Ready),
        Some(v) if v > VERSION => Ok(Prepared::Newer(v)),
        // No version (fresh file, or a crash before the version was
        // written) — and, later, older versions: v1 is the first, so there
        // is nothing to migrate from yet.
        _ => {
            conn.execute_batch(V1)?;
            set(conn, "version", &VERSION.to_string())?;
            Ok(Prepared::Empty)
        }
    }
}

pub fn get(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM schema_meta WHERE key = ?1",
        [key],
        |row| row.get(0),
    )
    .optional()
}

pub fn set(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO schema_meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [key, value],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_database_gets_the_schema_once() {
        let conn = Connection::open_in_memory().unwrap();
        assert_eq!(prepare(&conn).unwrap(), Prepared::Empty);
        assert_eq!(prepare(&conn).unwrap(), Prepared::Ready);
        assert_eq!(get(&conn, "version").unwrap().as_deref(), Some("1"));
    }

    #[test]
    fn newer_schema_is_reported_not_touched() {
        let conn = Connection::open_in_memory().unwrap();
        prepare(&conn).unwrap();
        set(&conn, "version", "99").unwrap();
        assert_eq!(prepare(&conn).unwrap(), Prepared::Newer(99));
        assert_eq!(get(&conn, "version").unwrap().as_deref(), Some("99"));
    }
}
