//! Append-only ledger of per-turn cost.
//!
//! Every completed turn records one row: the session and source that
//! drove it, the model that billed it, the summed token counts, and the
//! charged cost. Rows are stamped with the build's git revision so a
//! cost shift can be attributed to the change that caused it.
//!
//! This is telemetry, not core state: a write failure is logged by the
//! caller and never fails the turn.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, params};

use crate::agent::TurnUsage;

/// The build's git revision, injected by the flake at compile time.
/// `None` in plain `cargo` dev builds, where the env var is unset.
const GIT_SHA: Option<&str> = option_env!("GIT_SHA");

/// One turn's context, paired with its billed [`TurnUsage`] at write
/// time. Borrowed — nothing is retained past the insert.
pub struct TurnRecord<'a> {
    pub session: &'a str,
    pub source: &'a str,
    pub model: &'a str,
    pub usage: TurnUsage,
}

/// Append-only `SQLite` ledger of per-turn usage.
pub struct UsageLedger {
    conn: Mutex<Connection>,
}

impl UsageLedger {
    /// Open (or create) the ledger at `path` and ensure its table
    /// exists.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA busy_timeout = 30000;
             CREATE TABLE IF NOT EXISTS turns (
                 id                INTEGER PRIMARY KEY,
                 recorded_at       TEXT    NOT NULL DEFAULT (datetime('now')),
                 git_sha           TEXT,
                 session           TEXT    NOT NULL,
                 source            TEXT    NOT NULL,
                 model             TEXT    NOT NULL,
                 calls             INTEGER NOT NULL,
                 prompt_tokens     INTEGER NOT NULL,
                 completion_tokens INTEGER NOT NULL,
                 cost              REAL
             );",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Append one turn.
    pub fn record(&self, turn: &TurnRecord) -> rusqlite::Result<()> {
        let conn = self.conn.lock().expect("usage ledger mutex poisoned");
        conn.execute(
            "INSERT INTO turns
                 (git_sha, session, source, model,
                  calls, prompt_tokens, completion_tokens, cost)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                GIT_SHA,
                turn.session,
                turn.source,
                turn.model,
                turn.usage.calls,
                turn.usage.prompt_tokens.cast_signed(),
                turn.usage.completion_tokens.cast_signed(),
                turn.usage.cost,
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_temp() -> (tempfile::TempDir, UsageLedger) {
        let dir = tempfile::tempdir().unwrap();
        let ledger = UsageLedger::open(&dir.path().join("usage.db")).unwrap();
        (dir, ledger)
    }

    #[test]
    fn records_a_turn_row() {
        let (_dir, ledger) = open_temp();
        ledger
            .record(&TurnRecord {
                session: "general",
                source: "socket",
                model: "z-ai/glm-5.2",
                usage: TurnUsage {
                    calls: 3,
                    prompt_tokens: 1500,
                    completion_tokens: 200,
                    cost: Some(0.0042),
                },
            })
            .unwrap();

        let conn = ledger.conn.lock().unwrap();
        let (session, source, model, calls, prompt, completion, cost): (
            String,
            String,
            String,
            i64,
            i64,
            i64,
            Option<f64>,
        ) = conn
            .query_row(
                "SELECT session, source, model, calls, prompt_tokens, \
                 completion_tokens, cost FROM turns",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(session, "general");
        assert_eq!(source, "socket");
        assert_eq!(model, "z-ai/glm-5.2");
        assert_eq!(calls, 3);
        assert_eq!(prompt, 1500);
        assert_eq!(completion, 200);
        assert_eq!(cost, Some(0.0042));
    }

    #[test]
    fn null_cost_when_provider_reports_none() {
        let (_dir, ledger) = open_temp();
        ledger
            .record(&TurnRecord {
                session: "s",
                source: "telegram",
                model: "m",
                usage: TurnUsage {
                    calls: 1,
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    cost: None,
                },
            })
            .unwrap();
        let conn = ledger.conn.lock().unwrap();
        let cost: Option<f64> = conn
            .query_row("SELECT cost FROM turns", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cost, None);
    }

    #[test]
    fn append_only_accumulates_rows() {
        let (_dir, ledger) = open_temp();
        for _ in 0..3 {
            ledger
                .record(&TurnRecord {
                    session: "s",
                    source: "socket",
                    model: "m",
                    usage: TurnUsage::default(),
                })
                .unwrap();
        }
        let conn = ledger.conn.lock().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3);
    }
}
