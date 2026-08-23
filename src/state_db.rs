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
const MIGRATIONS: &[&str] = &[
    include_str!("state_db/migrations/0001_baseline.sql"),
    include_str!("state_db/migrations/0002_task.sql"),
    include_str!("state_db/migrations/0003_turn_timing.sql"),
];

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

    /// Read the named state document. `None` if never written.
    pub fn get_doc(&self, name: &str) -> rusqlite::Result<Option<String>> {
        use rusqlite::OptionalExtension;
        let conn = self.conn.lock().expect("state db mutex poisoned");
        conn.query_row("SELECT value FROM docs WHERE name = ?1", [name], |r| {
            r.get(0)
        })
        .optional()
    }

    /// Load a JSON state document, falling back on `fallback` when the
    /// document is missing, unreadable, or corrupt. Loss of any cursor
    /// document is benign by design — every owner degrades to
    /// starting-from-now semantics.
    pub fn load_json<T: serde::de::DeserializeOwned>(
        &self,
        name: &str,
        fallback: impl FnOnce() -> T,
    ) -> T {
        match self.get_doc(name) {
            Ok(Some(json)) => serde_json::from_str(&json).unwrap_or_else(|e| {
                tracing::warn!(doc = name, "Corrupt state document, starting fresh: {e}");
                fallback()
            }),
            Ok(None) => fallback(),
            Err(e) => {
                tracing::warn!(
                    doc = name,
                    "Failed to read state document, starting fresh: {e}"
                );
                fallback()
            }
        }
    }

    /// Persist a JSON state document. Failure is logged, not fatal —
    /// the owner retries at its next save point.
    pub fn save_json<T: serde::Serialize>(&self, name: &str, value: &T) {
        match serde_json::to_string(value) {
            Ok(json) => {
                if let Err(e) = self.put_doc(name, &json) {
                    tracing::error!(doc = name, "Failed to write state document: {e}");
                }
            }
            Err(e) => tracing::error!(doc = name, "Failed to serialize state document: {e}"),
        }
    }

    /// Write (upsert) the named state document.
    pub fn put_doc(&self, name: &str, value: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().expect("state db mutex poisoned");
        conn.execute(
            "INSERT INTO docs (name, value, updated_at)
             VALUES (?1, ?2, datetime('now'))
             ON CONFLICT(name) DO UPDATE
             SET value = excluded.value, updated_at = excluded.updated_at",
            rusqlite::params![name, value],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every ledger query must prepare against the migrated schema:
    /// column drift between an INSERT and its SELECT, or between a
    /// query and the ladder, fails here at `just check` instead of at
    /// runtime (the FUTURE.md prepare-all-queries item).
    #[test]
    fn all_ledger_queries_prepare_against_migrated_schema() {
        let db = StateDb::open_in_memory().unwrap();
        let conn = db.connection();
        let conn = conn.lock().unwrap();
        for sql in [
            crate::usage::INSERT_TURN,
            crate::usage::SELECT_TURN_ROWS,
            crate::review::INSERT_REVIEW,
            crate::review::INSERT_SELF_FINDING,
            crate::review::INSERT_EXTERNAL_FINDING,
            crate::review::UPDATE_DISPOSITION,
            crate::review::SELECT_REVIEWS_BY_GATE,
            crate::review::SELECT_FINDINGS_BY_CATEGORY,
            crate::review::SELECT_DISPOSITIONS_BY_SOURCE,
        ] {
            conn.prepare(sql)
                .unwrap_or_else(|e| panic!("query failed to prepare: {e}\n{sql}"));
        }
    }

    /// A live database predates the task column; its rows must survive
    /// the ladder and read back with a NULL task.
    #[test]
    fn legacy_rows_survive_the_task_migration() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::sqlite::apply_migrations(&conn, &MIGRATIONS[..1]).unwrap();
        conn.execute(
            "INSERT INTO turns (session, source, model, calls,
                                prompt_tokens, completion_tokens)
             VALUES ('s', 'Socket', 'm', 1, 10, 5)",
            [],
        )
        .unwrap();
        crate::sqlite::apply_migrations(&conn, MIGRATIONS).unwrap();
        let task: Option<String> = conn
            .query_row("SELECT task FROM turns", [], |r| r.get(0))
            .unwrap();
        assert_eq!(task, None);
    }

    #[test]
    fn open_migrates_to_current_version() {
        let db = StateDb::open_in_memory().unwrap();
        let conn = db.connection();
        let conn = conn.lock().unwrap();
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, i64::try_from(MIGRATIONS.len()).unwrap());
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
    fn docs_roundtrip_and_upsert() {
        let db = StateDb::open_in_memory().unwrap();
        assert_eq!(db.get_doc("duties").unwrap(), None);
        db.put_doc("duties", "{\"a\":1}").unwrap();
        db.put_doc("duties", "{\"a\":2}").unwrap();
        assert_eq!(db.get_doc("duties").unwrap().as_deref(), Some("{\"a\":2}"));
    }

    #[test]
    fn reopen_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kitaebot.db");
        drop(StateDb::open(&path).unwrap());
        drop(StateDb::open(&path).unwrap());
    }
}
