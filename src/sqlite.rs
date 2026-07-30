//! Shared `SQLite` migration runner.
//!
//! Migrations are tracked via `PRAGMA user_version`: entry `i` of the
//! migration slice brings the schema from version `i` to `i + 1`. Each
//! entry runs inside its own `BEGIN EXCLUSIVE` block together with the
//! matching version bump, so a failure rolls back atomically and
//! concurrent openers cannot interleave — the loser of the lock race
//! wakes up to a no-op once the winner has advanced the version.
//!
//! Adding a migration: drop a new `NNNN_*.sql` file in the owner's
//! `migrations/` directory and append it to its slice. Never reorder,
//! edit, or remove an existing entry — that breaks every database that
//! already advanced past it.

use rusqlite::Connection;

/// Apply pending migrations from `migrations`, advancing
/// `PRAGMA user_version` after each one.
pub fn apply_migrations(conn: &Connection, migrations: &[&str]) -> rusqlite::Result<()> {
    let current: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
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
            return Err(e);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_version(conn: &Connection) -> i32 {
        conn.pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap()
    }

    /// Already-applied entries are skipped — a second pass with one
    /// appended migration runs only the new entry.
    #[test]
    fn skips_already_applied() {
        let conn = Connection::open_in_memory().unwrap();
        let v1 = "CREATE TABLE first (id INTEGER PRIMARY KEY);";
        apply_migrations(&conn, &[v1]).unwrap();

        let v2 = "CREATE TABLE second (id INTEGER PRIMARY KEY);";
        apply_migrations(&conn, &[v1, v2]).unwrap();

        assert_eq!(user_version(&conn), 2);
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('first', 'second')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
    }

    /// A failing migration rolls back atomically — the version stays
    /// at the previous value and partial DDL does not leak.
    #[test]
    fn rolls_back_on_error() {
        let conn = Connection::open_in_memory().unwrap();
        let broken = "CREATE TABLE leftover (id INTEGER); INSERT INTO no_such_table VALUES (1);";
        assert!(apply_migrations(&conn, &[broken]).is_err());

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
