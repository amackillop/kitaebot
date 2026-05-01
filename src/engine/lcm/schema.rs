//! `SQLite` connection bootstrap for the LCM engine.
//!
//! [`open`] opens (or creates) the database at the given path, applies
//! the tuned PRAGMA set, runs any pending migrations, and registers the
//! `REGEXP` user function backed by the `regex` crate.
//!
//! Migrations are tracked via `PRAGMA user_version`. [`MIGRATIONS`] is
//! an ordered slice — entry `i` brings the schema from version `i` to
//! `i + 1`. Each entry runs inside its own `BEGIN EXCLUSIVE` block
//! together with the matching `PRAGMA user_version = i + 1` so a
//! failure rolls the version back. Concurrent processes cannot
//! interleave migrations — the loser of the lock race wakes up to a
//! no-op once the winner has bumped the version past its slice.
//!
//! Adding a migration: drop a new `NNNN_*.sql` file in `migrations/`
//! and append it to [`MIGRATIONS`]. Never reorder, edit, or remove an
//! existing entry — that breaks every database that already advanced
//! past it. The pre-commit hook enforces append-only on the files in
//! that directory.

use std::path::Path;

use regex::Regex;
use rusqlite::Connection;
use rusqlite::functions::FunctionFlags;

use crate::error::EngineError;

/// PRAGMAs applied on every connection open.
///
/// Defaults are not sufficient: the 2 MiB cache thrashes on long conversations,
/// and `busy_timeout = 0` produces immediate `SQLITE_BUSY` failures the
/// moment a reader and writer touch the DB at once.
const PRAGMAS: &str = "\
PRAGMA journal_mode = WAL;
PRAGMA busy_timeout = 30000;
PRAGMA foreign_keys = ON;
PRAGMA cache_size = -65536;
PRAGMA synchronous = NORMAL;
PRAGMA temp_store = MEMORY;
";

/// Ordered list of schema migrations.
///
/// Entry `i` brings the database from version `i` to version `i + 1`.
/// The first entry is the v1 baseline. Append new migrations; do not
/// reorder, edit, or remove existing entries.
const MIGRATIONS: &[&str] = &[include_str!("migrations/0001_baseline.sql")];

/// Open or create the LCM database at `path`.
///
/// Applies PRAGMAs, runs any pending migrations, and registers the
/// `REGEXP` user function. Idempotent.
///
/// # Errors
///
/// Returns [`EngineError::Storage`] if the file cannot be opened, a
/// migration fails, or the user function cannot be registered.
pub fn open(path: &Path) -> Result<Connection, EngineError> {
    let conn = Connection::open(path).map_err(|e| storage_err(&e))?;
    init(&conn)?;
    Ok(conn)
}

/// Apply pragmas, migrations, and user functions to an open connection.
///
/// Exposed separately so tests can use `:memory:` connections without
/// needing a temp file on disk.
///
/// # Errors
///
/// Returns [`EngineError::Storage`] on any underlying `SQLite` failure.
pub fn init(conn: &Connection) -> Result<(), EngineError> {
    conn.execute_batch(PRAGMAS).map_err(|e| storage_err(&e))?;
    apply_migrations(conn, MIGRATIONS)?;
    register_regexp(conn)?;
    Ok(())
}

/// Apply pending migrations from `migrations`, advancing
/// `PRAGMA user_version` after each one.
///
/// Each migration runs inside its own `BEGIN EXCLUSIVE; ...; COMMIT;`
/// block paired with `PRAGMA user_version = N`, so a partial failure
/// rolls back atomically and leaves the version at its previous value.
fn apply_migrations(conn: &Connection, migrations: &[&str]) -> Result<(), EngineError> {
    let current: i32 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|e| storage_err(&e))?;
    let start = usize::try_from(current).unwrap_or(0);

    for (i, sql) in migrations.iter().enumerate().skip(start) {
        let target = i + 1;
        let stmt = format!("BEGIN EXCLUSIVE;\n{sql}\nPRAGMA user_version = {target};\nCOMMIT;");
        if let Err(e) = conn.execute_batch(&stmt) {
            // SQLite does not implicitly rollback on a statement-level
            // error mid-transaction; the BEGIN above stays open and any
            // DDL run before the failure remains visible to this
            // connection. Force the rollback so partial migrations do
            // not leak into subsequent attempts.
            let _ = conn.execute_batch("ROLLBACK;");
            return Err(storage_err(&e));
        }
    }
    Ok(())
}

/// Register `REGEXP(pattern, text) -> bool` on the connection.
///
/// `SQLite` exposes a `REGEXP` operator (`text REGEXP pattern`) that
/// dispatches to a user-defined function of the same name. We register
/// a Rust regex implementation. The function is deterministic — the
/// same `(pattern, text)` always returns the same result — so `SQLite`
/// is free to cache it across rows.
///
/// The pattern is recompiled per call. Optimizing this with rusqlite's
/// `auxdata` is a follow-up once `lcm_grep` is in heavy use.
fn register_regexp(conn: &Connection) -> Result<(), EngineError> {
    conn.create_scalar_function(
        "regexp",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let pattern: String = ctx.get(0)?;
            let text: String = ctx.get(1)?;
            let re = Regex::new(&pattern)
                .map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e)))?;
            Ok(re.is_match(&text))
        },
    )
    .map_err(|e| storage_err(&e))
}

fn storage_err(e: &rusqlite::Error) -> EngineError {
    EngineError::Storage(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        conn
    }

    /// Every named table from the spec data model is present after init.
    #[test]
    fn init_creates_all_tables() {
        let conn = fresh();
        let expected = [
            "conversations",
            "messages",
            "message_parts",
            "summaries",
            "summary_messages",
            "summary_parents",
            "large_files",
            "summary_files",
            "context_items",
            "messages_fts",
            "summaries_fts",
        ];
        for name in expected {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master \
                     WHERE name = ?1 AND type IN ('table','view')",
                    [name],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "missing table: {name}");
        }
    }

    /// Re-running `init` against a populated DB is a no-op (no errors,
    /// existing data preserved).
    #[test]
    fn init_is_idempotent() {
        let conn = fresh();
        conn.execute(
            "INSERT INTO conversations(name, created_at, updated_at) \
             VALUES ('general', '2025-01-01', '2025-01-01')",
            [],
        )
        .unwrap();

        init(&conn).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM conversations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    /// Foreign keys are enforced — inserting a message with a bogus
    /// conversation id fails.
    #[test]
    fn foreign_keys_enforced() {
        let conn = fresh();
        let result = conn.execute(
            "INSERT INTO messages(conversation_id, seq, role, content, token_count, created_at) \
             VALUES (999, 0, 'user', 'hi', 1, '2025-01-01')",
            [],
        );
        assert!(result.is_err(), "expected FK violation, got {result:?}");
    }

    /// Inserting into `messages` propagates content into the FTS index
    /// via the `messages_ai` trigger; an FTS query finds it.
    #[test]
    fn messages_fts_indexed_via_trigger() {
        let conn = fresh();
        conn.execute(
            "INSERT INTO conversations(name, created_at, updated_at) \
             VALUES ('general', '2025-01-01', '2025-01-01')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages(conversation_id, seq, role, content, token_count, created_at) \
             VALUES (1, 0, 'user', 'the quick brown fox', 5, '2025-01-01')",
            [],
        )
        .unwrap();

        let hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'fox'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1);
    }

    /// Summaries trigger maintains the standalone FTS table including
    /// the unindexed `summary_id` column.
    #[test]
    fn summaries_fts_indexed_via_trigger() {
        let conn = fresh();
        conn.execute(
            "INSERT INTO conversations(name, created_at, updated_at) \
             VALUES ('general', '2025-01-01', '2025-01-01')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO summaries( \
                summary_id, conversation_id, kind, depth, content, token_count, \
                earliest_at, latest_at, descendant_count, descendant_token_count, \
                source_message_token_count, model, created_at \
             ) VALUES ( \
                'sum_abc123', 1, 'leaf', 0, 'discussion of widgets', 3, \
                '2025-01-01', '2025-01-01', 1, 3, 10, 'mock', '2025-01-01' \
             )",
            [],
        )
        .unwrap();

        let id: String = conn
            .query_row(
                "SELECT summary_id FROM summaries_fts WHERE summaries_fts MATCH 'widgets'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(id, "sum_abc123");
    }

    /// `REGEXP` operator dispatches to the registered Rust regex impl.
    #[test]
    fn regexp_function_matches() {
        let conn = fresh();
        let matched: bool = conn
            .query_row("SELECT 'hello world' REGEXP '^hello'", [], |row| row.get(0))
            .unwrap();
        assert!(matched);

        let not_matched: bool = conn
            .query_row("SELECT 'hello world' REGEXP '^world'", [], |row| row.get(0))
            .unwrap();
        assert!(!not_matched);
    }

    /// Invalid regex pattern surfaces as a `SQLite` error rather than
    /// crashing the connection.
    #[test]
    fn regexp_invalid_pattern_errors() {
        let conn = fresh();
        let result: Result<bool, _> = conn.query_row("SELECT 'x' REGEXP '['", [], |row| row.get(0));
        assert!(result.is_err());
    }

    fn user_version(conn: &Connection) -> i32 {
        conn.pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap()
    }

    /// `init` advances `user_version` to the number of bundled
    /// migrations.
    #[test]
    fn init_sets_user_version() {
        let conn = fresh();
        assert_eq!(
            user_version(&conn),
            i32::try_from(MIGRATIONS.len()).unwrap()
        );
    }

    /// `apply_migrations` skips entries already applied — running with
    /// the production slice on an in-memory DB twice does not re-run
    /// the v1 baseline.
    #[test]
    fn apply_migrations_skips_already_applied() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(PRAGMAS).unwrap();
        apply_migrations(&conn, MIGRATIONS).unwrap();

        // A second pass with a stub follow-up migration should run only
        // the stub and bump the version by exactly one.
        let stub = "CREATE TABLE migration_probe (id INTEGER PRIMARY KEY);";
        let extended: Vec<&str> = MIGRATIONS
            .iter()
            .copied()
            .chain(std::iter::once(stub))
            .collect();
        apply_migrations(&conn, &extended).unwrap();

        assert_eq!(user_version(&conn), i32::try_from(extended.len()).unwrap());
        let probe_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'migration_probe'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(probe_count, 1);
    }

    /// A failing migration rolls back atomically — the version stays
    /// at the previous value and partial DDL does not leak.
    #[test]
    fn apply_migrations_rolls_back_on_error() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(PRAGMAS).unwrap();

        let broken = "CREATE TABLE leftover (id INTEGER); INSERT INTO no_such_table VALUES (1);";
        let result = apply_migrations(&conn, &[broken]);
        assert!(result.is_err());

        assert_eq!(user_version(&conn), 0);
        let leftover_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'leftover'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(leftover_count, 0, "partial DDL should have rolled back");
    }
}
