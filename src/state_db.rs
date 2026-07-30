//! Operational state database: `state/kitaebot.db`.
//!
//! One `SQLite` file for everything operational that is not the
//! context engine: the usage ledger, the review ledger, and the doc
//! store for cursor state. `lcm.db` stays the engine's own — hot
//! path, engine-lifecycle migrations — while this file collects the
//! low-rate ledgers so open, migrate, and backup happen once.
//!
//! One shared connection behind a mutex: writers here run a few times
//! per minute at most, so a single writer path beats per-subsystem
//! connections negotiating `SQLITE_BUSY` between themselves.
//!
//! Migrations follow the LCM scheme ([`crate::sqlite`]): tracked via
//! `PRAGMA user_version`, append-only `migrations/NNNN_*.sql`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

/// PRAGMAs applied at open. WAL plus a generous busy timeout: the
/// engine's read-only connections never touch this file, but backup's
/// `VACUUM INTO` can briefly contend with a writer.
const PRAGMAS: &str = "\
PRAGMA journal_mode = WAL;
PRAGMA busy_timeout = 30000;
PRAGMA foreign_keys = ON;
PRAGMA synchronous = NORMAL;
";

/// Ordered list of schema migrations. Entry `i` brings the database
/// from version `i` to `i + 1`. Append-only; never reorder or edit.
const MIGRATIONS: &[&str] = &[include_str!("state_db/migrations/0001_baseline.sql")];

/// Shared handle to the operational state database.
#[derive(Clone)]
pub struct StateDb {
    conn: Arc<Mutex<Connection>>,
}

impl StateDb {
    /// Open (or create) the database at `path`, applying PRAGMAs and
    /// any pending migrations.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        Self::init(Connection::open(path)?)
    }

    /// In-memory database for tests, same schema and PRAGMAs.
    #[cfg(test)]
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> rusqlite::Result<Self> {
        conn.execute_batch(PRAGMAS)?;
        crate::sqlite::apply_migrations(&conn, MIGRATIONS)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// The shared connection. Consumers (ledgers, doc store) lock per
    /// statement; the schema is guaranteed by [`StateDb::open`].
    pub(crate) fn connection(&self) -> Arc<Mutex<Connection>> {
        Arc::clone(&self.conn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_creates_schema_at_version_one() {
        let db = StateDb::open_in_memory().unwrap();
        let conn = db.connection();
        let conn = conn.lock().unwrap();
        let version: i32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 1);
        for table in ["turns", "reviews", "findings", "docs"] {
            let count: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "missing table {table}");
        }
    }

    #[test]
    fn reopen_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kitaebot.db");
        drop(StateDb::open(&path).unwrap());
        drop(StateDb::open(&path).unwrap());
    }
}
