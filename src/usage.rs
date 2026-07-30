//! Append-only ledger of per-turn cost.
//!
//! Every completed turn records one row: the session and source that
//! drove it, the model that billed it, the summed token counts, and the
//! charged cost. Rows are stamped with the build's git revision so a
//! cost shift can be attributed to the change that caused it.
//!
//! This is telemetry, not core state: a write failure is logged by the
//! caller and never fails the turn.

use std::cmp::Ordering;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, params};

use crate::state_db::StateDb;
use tracing::warn;

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

/// Append-only ledger of per-turn usage, on the shared operational
/// database ([`StateDb`]); the `turns` schema lives in its baseline
/// migration.
pub struct UsageLedger {
    conn: Arc<Mutex<Connection>>,
}

impl UsageLedger {
    pub fn new(db: &StateDb) -> Self {
        Self {
            conn: db.connection(),
        }
    }

    /// Read every recorded turn, projected to the columns the report
    /// aggregates. The ledger is prunable, so an unbounded read is fine.
    pub fn rows(&self) -> rusqlite::Result<Vec<TurnRow>> {
        let conn = self.conn.lock().expect("usage ledger mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT git_sha, model, prompt_tokens, completion_tokens, cost
                 FROM turns ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(TurnRow {
                    git_sha: r.get(0)?,
                    model: r.get(1)?,
                    prompt_tokens: r.get::<_, i64>(2)?.cast_unsigned(),
                    completion_tokens: r.get::<_, i64>(3)?.cast_unsigned(),
                    cost: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
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

/// Record a turn to the ledger if one is configured. A write failure is
/// logged, never propagated — telemetry must not fail the turn.
pub fn record_turn(ledger: Option<&UsageLedger>, record: &TurnRecord) {
    if let Some(ledger) = ledger
        && let Err(e) = ledger.record(record)
    {
        warn!("Failed to record turn usage: {e}");
    }
}

/// One ledger row projected to the columns [`report`] aggregates.
pub struct TurnRow {
    pub git_sha: Option<String>,
    pub model: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cost: Option<f64>,
}

/// Running totals over a group of turns.
#[derive(Default, Clone)]
struct Agg {
    turns: u64,
    tokens: u64,
    cost: f64,
    /// At least one turn in the group reported a cost. When false the
    /// cost column shows "-": the provider never billed, so 0 would lie.
    metered: bool,
}

impl Agg {
    fn add(&mut self, row: &TurnRow) {
        self.turns += 1;
        self.tokens += row.prompt_tokens + row.completion_tokens;
        if let Some(cost) = row.cost {
            self.cost += cost;
            self.metered = true;
        }
    }
}

/// Group turns by `key` in first-seen order. Rows arrive in insertion
/// order, so first-seen is chronological.
fn group_by(rows: &[TurnRow], key: impl Fn(&TurnRow) -> String) -> Vec<(String, Agg)> {
    let mut groups: Vec<(String, Agg)> = Vec::new();
    for row in rows {
        let k = key(row);
        if let Some((_, agg)) = groups.iter_mut().find(|(name, _)| *name == k) {
            agg.add(row);
        } else {
            let mut agg = Agg::default();
            agg.add(row);
            groups.push((k, agg));
        }
    }
    groups
}

/// Render the `/usage` report: totals, then a per-build and per-model
/// breakdown. The per-build view is the point — it attributes a cost
/// shift to the change that shipped it.
pub fn report(rows: &[TurnRow]) -> String {
    if rows.is_empty() {
        return "No usage recorded yet.".to_string();
    }

    let mut total = Agg::default();
    for row in rows {
        total.add(row);
    }

    let mut out = String::new();
    let _ = writeln!(
        out,
        "Usage ({} turns, {})\n",
        total.turns,
        fmt_cost(total.cost, total.metered),
    );

    // Chronological: a cost shift reads as a timeline of deploys.
    let build = group_by(rows, |r| {
        r.git_sha
            .as_deref()
            .map_or("unknown".to_string(), short_sha)
    });
    write_table(&mut out, "By Build", "Build", &build, true);

    let mut model = group_by(rows, |r| r.model.clone());
    model.sort_by(|a, b| {
        b.1.cost
            .partial_cmp(&a.1.cost)
            .unwrap_or(Ordering::Equal)
            .then(b.1.tokens.cmp(&a.1.tokens))
    });
    write_table(&mut out, "By Model", "Model", &model, false);

    out
}

/// A per-group table. `per_turn` adds a $/turn column (useful per build,
/// noise per model).
fn write_table(
    out: &mut String,
    title: &str,
    label: &str,
    groups: &[(String, Agg)],
    per_turn: bool,
) {
    let _ = writeln!(out, "{title}\n");
    if per_turn {
        let _ = writeln!(
            out,
            "{label:<24} {:>6} {:>10} {:>12} {:>10}",
            "Turns", "Tokens", "Cost", "$/turn"
        );
    } else {
        let _ = writeln!(
            out,
            "{label:<24} {:>6} {:>10} {:>12}",
            "Turns", "Tokens", "Cost"
        );
    }
    for (name, agg) in groups {
        let cost = fmt_cost(agg.cost, agg.metered);
        if per_turn {
            let per = if agg.metered && agg.turns > 0 {
                #[allow(clippy::cast_precision_loss)]
                let turns = agg.turns as f64;
                format!("${:.4}", agg.cost / turns)
            } else {
                "-".to_string()
            };
            let _ = writeln!(
                out,
                "{name:<24} {:>6} {:>10} {cost:>12} {per:>10}",
                agg.turns,
                fmt_count(agg.tokens),
            );
        } else {
            let _ = writeln!(
                out,
                "{name:<24} {:>6} {:>10} {cost:>12}",
                agg.turns,
                fmt_count(agg.tokens),
            );
        }
    }
    out.push('\n');
}

/// First 7 hex characters, matching git's short-SHA convention.
fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// `$0.0000`, or `-` when the group was never billed a cost.
fn fmt_cost(cost: f64, metered: bool) -> String {
    if metered {
        format!("${cost:.4}")
    } else {
        "-".to_string()
    }
}

/// Compact token count: `1.2M`, `500.0K`, or the raw number below 1K.
fn fmt_count(n: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let f = n as f64;
    if n >= 1_000_000 {
        format!("{:.1}M", f / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", f / 1_000.0)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_temp() -> (tempfile::TempDir, UsageLedger) {
        let dir = tempfile::tempdir().unwrap();
        let ledger = UsageLedger::new(
            &crate::state_db::StateDb::open(&dir.path().join("kitaebot.db")).unwrap(),
        );
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

    fn row(sha: Option<&str>, model: &str, tokens: u64, cost: Option<f64>) -> TurnRow {
        TurnRow {
            git_sha: sha.map(String::from),
            model: model.to_string(),
            prompt_tokens: tokens,
            completion_tokens: 0,
            cost,
        }
    }

    #[test]
    fn report_empty_is_a_notice() {
        assert_eq!(report(&[]), "No usage recorded yet.");
    }

    #[test]
    fn report_totals_and_groups() {
        let rows = vec![
            row(Some("abcdef1234"), "glm", 1000, Some(0.01)),
            row(Some("abcdef1234"), "kimi", 500, Some(0.02)),
            row(Some("9999999999"), "glm", 2000, Some(0.05)),
        ];
        let out = report(&rows);
        // Header total: 3 turns, summed cost.
        assert!(out.contains("Usage (3 turns, $0.0800)"));
        // Short SHA, not the full hash.
        assert!(out.contains("abcdef1"));
        assert!(!out.contains("abcdef1234"));
        // Both axes present.
        assert!(out.contains("By Build"));
        assert!(out.contains("By Model"));
        // Per-model aggregation folds the two glm rows.
        assert!(out.contains("glm"));
        assert!(out.contains("kimi"));
    }

    #[test]
    fn builds_listed_chronologically_not_by_cost() {
        // The older build costs more; it must still come first.
        let rows = vec![
            row(Some("aaaaaaa111"), "glm", 100, Some(0.90)),
            row(Some("bbbbbbb222"), "glm", 100, Some(0.01)),
        ];
        let out = report(&rows);
        let old = out.find("aaaaaaa").unwrap();
        let new = out.find("bbbbbbb").unwrap();
        assert!(old < new);
    }

    #[test]
    fn report_unmetered_shows_dash() {
        let out = report(&[row(None, "local", 10, None)]);
        assert!(out.contains("Usage (1 turns, -)"));
        assert!(out.contains("unknown"));
    }

    #[test]
    fn rows_round_trip_through_the_ledger() {
        let (_dir, ledger) = open_temp();
        ledger
            .record(&TurnRecord {
                session: "s",
                source: "socket",
                model: "m",
                usage: TurnUsage {
                    calls: 1,
                    prompt_tokens: 42,
                    completion_tokens: 8,
                    cost: Some(0.5),
                },
            })
            .unwrap();
        let rows = ledger.rows().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].prompt_tokens, 42);
        assert_eq!(rows[0].completion_tokens, 8);
        assert_eq!(rows[0].cost, Some(0.5));
    }

    #[test]
    fn fmt_count_scales() {
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1_500), "1.5K");
        assert_eq!(fmt_count(2_400_000), "2.4M");
    }

    #[test]
    fn short_sha_takes_seven() {
        assert_eq!(short_sha("0123456789abcdef"), "0123456");
        assert_eq!(short_sha("abc"), "abc");
    }
}
