//! Self-review findings: block parsing and the review ledger.
//!
//! See `specs/23-self-review.md`. Reviewer sub-agents end every
//! response with a fenced `findings` block holding one JSON object;
//! the task tool parses it here and records rows mechanically. The
//! ledger is telemetry like the usage ledger: a write failure is
//! logged by the caller and never fails the review.

use std::fmt::Write as _;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, params};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::error::ToolError;
use crate::tools::{Tool, ToolCtx};

/// The parsed findings block from a reviewer response.
#[derive(Debug, Deserialize, PartialEq)]
pub struct ReviewOutput {
    pub verdict: Verdict,
    #[serde(default)]
    pub confidence: Option<f64>,
    #[serde(default)]
    pub explanation: String,
    #[serde(default)]
    pub findings: Vec<Finding>,
}

/// Whether the artifact is free of blocking issues, ignoring nits.
#[derive(Debug, Deserialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Correct,
    Incorrect,
}

impl Verdict {
    fn as_str(self) -> &'static str {
        match self {
            Self::Correct => "correct",
            Self::Incorrect => "incorrect",
        }
    }
}

/// One finding. Category and severity stay free text at this layer:
/// the taxonomy is deliberately open (spec 23), and a novel category
/// must not invalidate the row that carries it.
#[derive(Debug, Deserialize, PartialEq)]
pub struct Finding {
    pub category: String,
    #[serde(default)]
    pub severity: Option<String>,
    #[serde(default)]
    pub confidence: Option<f64>,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub line: Option<i64>,
    #[serde(default)]
    pub note: String,
}

/// Extract and parse the fenced `findings` block from a reviewer
/// response. Returns `None` when the block is absent or malformed —
/// the caller warns and moves on; a review never fails on its
/// telemetry.
pub fn parse_findings_block(text: &str) -> Option<ReviewOutput> {
    let start = text.rfind("```findings")?;
    let body = &text[start + "```findings".len()..];
    let end = body.find("```")?;
    serde_json::from_str(body[..end].trim()).ok()
}

/// The review-gates segment appended to root system prompts when the
/// pipeline is enabled. Kept out of `AGENTS.md` so a disabled pipeline
/// leaves no prompt referencing unavailable mechanics.
pub const GATES_SEGMENT: &str = include_str!("prompts/review-gates.md");

/// Ledger row context for one gate invocation: which repo, which gate
/// (`plan` | `commit` | `series`), and the ref under review.
pub struct GateRecord<'a> {
    pub repo: &'a str,
    pub gate: &'a str,
    pub git_ref: &'a str,
}

/// Append-only `SQLite` ledger of review verdicts and findings.
///
/// `reviews` answers what finding rows cannot: whether a gate ran at
/// all. `findings` carries `source` so self findings and external
/// escapes share one query surface.
pub struct ReviewLedger {
    conn: Mutex<Connection>,
}

impl ReviewLedger {
    /// Open (or create) the ledger at `path` and ensure its tables
    /// exist.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA busy_timeout = 30000;
             CREATE TABLE IF NOT EXISTS reviews (
                 id          INTEGER PRIMARY KEY,
                 recorded_at TEXT NOT NULL DEFAULT (datetime('now')),
                 repo        TEXT NOT NULL,
                 gate        TEXT NOT NULL,
                 git_ref     TEXT NOT NULL,
                 verdict     TEXT NOT NULL,
                 confidence  REAL
             );
             CREATE TABLE IF NOT EXISTS findings (
                 id          INTEGER PRIMARY KEY,
                 recorded_at TEXT NOT NULL DEFAULT (datetime('now')),
                 repo        TEXT NOT NULL,
                 gate        TEXT NOT NULL,
                 git_ref     TEXT NOT NULL,
                 source      TEXT NOT NULL,
                 category    TEXT NOT NULL,
                 severity    TEXT,
                 confidence  REAL,
                 file        TEXT,
                 line        INTEGER,
                 note        TEXT NOT NULL,
                 disposition      TEXT,
                 disposition_note TEXT,
                 disposed_at      TEXT
             );",
        )?;
        // Deployed ledgers predate the disposition columns, and
        // CREATE TABLE IF NOT EXISTS never alters an existing table.
        for column in ["disposition", "disposition_note", "disposed_at"] {
            add_column_if_missing(&conn, "findings", column)?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Record one gate invocation: the review row plus one finding row
    /// per entry, all `source = 'self'`. One transaction; a gate either
    /// lands whole or not at all. Returns the inserted finding ids so
    /// the caller can surface them for disposition.
    pub fn record_review(
        &self,
        gate: &GateRecord,
        output: &ReviewOutput,
    ) -> rusqlite::Result<Vec<i64>> {
        let mut conn = self.conn.lock().expect("review ledger mutex poisoned");
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO reviews (repo, gate, git_ref, verdict, confidence)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                gate.repo,
                gate.gate,
                gate.git_ref,
                output.verdict.as_str(),
                output.confidence,
            ],
        )?;
        let mut ids = Vec::with_capacity(output.findings.len());
        {
            let mut ins = tx.prepare(
                "INSERT INTO findings
                     (repo, gate, git_ref, source, category, severity,
                      confidence, file, line, note)
                 VALUES (?1, ?2, ?3, 'self', ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for f in &output.findings {
                ins.execute(params![
                    gate.repo,
                    gate.gate,
                    gate.git_ref,
                    f.category,
                    f.severity,
                    f.confidence,
                    f.file,
                    f.line,
                    f.note,
                ])?;
                ids.push(tx.last_insert_rowid());
            }
        }
        tx.commit()?;
        Ok(ids)
    }

    /// Record one externally-sourced finding (a human or review-bot
    /// comment). Severity and confidence stay NULL: those are
    /// self-review signals. Returns the inserted finding id so the
    /// caller can surface it for disposition.
    pub fn record_finding(&self, f: &ExternalFinding) -> rusqlite::Result<i64> {
        let conn = self.conn.lock().expect("review ledger mutex poisoned");
        conn.execute(
            "INSERT INTO findings
                 (repo, gate, git_ref, source, category, file, line, note)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                f.repo, f.gate, f.git_ref, f.source, f.category, f.file, f.line, f.note,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Record the parent's decision on a finding. Returns `false` when
    /// no row carries `id` — the caller turns that into a visible tool
    /// error rather than a silent no-op.
    pub fn set_disposition(
        &self,
        id: i64,
        disposition: &str,
        note: Option<&str>,
    ) -> rusqlite::Result<bool> {
        let conn = self.conn.lock().expect("review ledger mutex poisoned");
        let updated = conn.execute(
            "UPDATE findings
             SET disposition = ?2, disposition_note = ?3,
                 disposed_at = datetime('now')
             WHERE id = ?1",
            params![id, disposition, note],
        )?;
        Ok(updated > 0)
    }

    /// Render the `/findings` report: reviews by gate and verdict,
    /// finding counts by category split self vs external, then
    /// dispositions by source.
    pub fn report(&self) -> rusqlite::Result<String> {
        let conn = self.conn.lock().expect("review ledger mutex poisoned");

        let reviews: Vec<(String, i64, i64)> = {
            let mut stmt = conn.prepare(
                "SELECT gate, COUNT(*),
                        SUM(CASE WHEN verdict = 'incorrect' THEN 1 ELSE 0 END)
                 FROM reviews GROUP BY gate ORDER BY gate",
            )?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };

        let categories: Vec<(String, i64, i64)> = {
            let mut stmt = conn.prepare(
                "SELECT category,
                        SUM(CASE WHEN source = 'self' THEN 1 ELSE 0 END),
                        SUM(CASE WHEN source != 'self' THEN 1 ELSE 0 END)
                 FROM findings GROUP BY category ORDER BY COUNT(*) DESC",
            )?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };

        // (source, total, fixed, disputed, no-action, pending). Dispute
        // rate per source is the query disposition tracking exists for;
        // pending measures disposition discipline itself (spec 23).
        let dispositions: Vec<(String, i64, i64, i64, i64, i64)> = {
            let mut stmt = conn.prepare(
                "SELECT source, COUNT(*),
                        SUM(CASE WHEN disposition = 'fixed' THEN 1 ELSE 0 END),
                        SUM(CASE WHEN disposition = 'disputed' THEN 1 ELSE 0 END),
                        SUM(CASE WHEN disposition = 'no-action' THEN 1 ELSE 0 END),
                        SUM(CASE WHEN disposition IS NULL THEN 1 ELSE 0 END)
                 FROM findings GROUP BY source ORDER BY source",
            )?;
            stmt.query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };

        if reviews.is_empty() && categories.is_empty() {
            return Ok("No reviews recorded.".to_string());
        }

        let mut out = String::from("Reviews by gate\n\n");
        let _ = writeln!(out, "{:<10} {:>8} {:>10}", "Gate", "Runs", "Incorrect");
        for (gate, runs, incorrect) in &reviews {
            let _ = writeln!(out, "{gate:<10} {runs:>8} {incorrect:>10}");
        }
        out.push_str("\nFindings by category (external = escapes)\n\n");
        let _ = writeln!(out, "{:<22} {:>6} {:>10}", "Category", "Self", "External");
        for (category, own, external) in &categories {
            let _ = writeln!(out, "{category:<22} {own:>6} {external:>10}");
        }
        if !dispositions.is_empty() {
            out.push_str("\nDispositions by source\n\n");
            let _ = writeln!(
                out,
                "{:<8} {:>6} {:>6} {:>9} {:>10} {:>8}",
                "Source", "Total", "Fixed", "Disputed", "No-action", "Pending",
            );
            for (source, total, fixed, disputed, no_action, pending) in &dispositions {
                let _ = writeln!(
                    out,
                    "{source:<8} {total:>6} {fixed:>6} {disputed:>9} {no_action:>10} {pending:>8}",
                );
            }
        }
        Ok(out)
    }
}

/// Add `column` to `table` when it is not already present.
fn add_column_if_missing(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(&format!(
        "SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1"
    ))?;
    let present: i64 = stmt.query_row([column], |r| r.get(0))?;
    if present == 0 {
        conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {column} TEXT"), [])?;
    }
    Ok(())
}

/// One externally-sourced finding, as recorded by [`ReviewLogTool`].
pub struct ExternalFinding<'a> {
    pub repo: &'a str,
    pub gate: &'a str,
    pub git_ref: &'a str,
    pub source: &'a str,
    pub category: &'a str,
    pub file: Option<&'a str>,
    pub line: Option<i64>,
    pub note: &'a str,
}

#[derive(Deserialize, JsonSchema)]
struct LogArgs {
    /// Repository, `owner/repo`.
    repo: String,
    /// Where the finding arrived: "plan" for corrections to a posted
    /// plan, "external" for PR review comments.
    gate: String,
    /// What the finding refers to: PR number, SHA, or branch.
    git_ref: String,
    /// Who raised it: "human" or "bot".
    source: String,
    /// Free-text category. Reuse an existing category name when one
    /// fits; coin a precise new one when none does.
    category: String,
    /// File the comment is anchored to, when it is.
    #[serde(default)]
    file: Option<String>,
    /// Line the comment is anchored to, when it is.
    #[serde(default)]
    line: Option<i64>,
    /// The finding itself, condensed to its substance.
    note: String,
}

/// Tool for logging externally-sourced review findings — the escapes
/// stream (spec 23). Root-only by construction: it appears in no
/// sub-agent allowlist.
pub struct ReviewLogTool {
    ledger: Arc<ReviewLedger>,
}

impl ReviewLogTool {
    pub fn new(ledger: Arc<ReviewLedger>) -> Self {
        Self { ledger }
    }
}

impl Tool for ReviewLogTool {
    fn name(&self) -> &'static str {
        "review_log"
    }

    fn description(&self) -> &'static str {
        "Log one externally-sourced review finding to the findings \
        ledger. Call it once per inline comment when processing PR \
        review feedback, and once per correction when a human amends a \
        posted plan — before acting on the feedback. Do not log your \
        own reviewer's findings; those are recorded automatically."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(LogArgs)).expect("schema serialization failed")
    }

    fn execute(
        &self,
        args: serde_json::Value,
        _ctx: ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let args: LogArgs = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
            // 'self' rows are written mechanically by the task tool;
            // a model-invoked write must not be able to forge them.
            if args.source == "self" {
                return Err(ToolError::InvalidArguments(
                    "source 'self' is reserved for mechanical recording".into(),
                ));
            }
            let id = self
                .ledger
                .record_finding(&ExternalFinding {
                    repo: &args.repo,
                    gate: &args.gate,
                    git_ref: &args.git_ref,
                    source: &args.source,
                    category: &args.category,
                    file: args.file.as_deref(),
                    line: args.line,
                    note: &args.note,
                })
                .map_err(|e| ToolError::ExecutionFailed(format!("review_log: {e}")))?;
            Ok(format!("Recorded finding #{id}."))
        })
    }
}

/// The parent's decision on a finding, after acting on it. Enum at
/// the tool boundary, free text in storage (spec 23).
#[derive(Deserialize, JsonSchema, Clone, Copy)]
#[serde(rename_all = "kebab-case")]
enum Disposition {
    Fixed,
    Disputed,
    NoAction,
}

impl Disposition {
    fn as_str(self) -> &'static str {
        match self {
            Self::Fixed => "fixed",
            Self::Disputed => "disputed",
            Self::NoAction => "no-action",
        }
    }
}

#[derive(Deserialize, JsonSchema)]
struct DispositionArgs {
    /// Ledger id of the finding, as surfaced when it was recorded.
    finding_id: i64,
    /// "fixed": code changed. "disputed": contested, note required.
    /// "no-action": uncontested but no change warranted (an ignored
    /// nit, an answered question).
    disposition: Disposition,
    /// The reason. Required for disputes.
    #[serde(default)]
    note: Option<String>,
}

/// Tool recording the parent's per-finding decision (spec 23,
/// Disposition tracking). Root-only by construction, like
/// [`ReviewLogTool`]. Annotates existing rows only, so it needs no
/// forgery guard.
pub struct ReviewDispositionTool {
    ledger: Arc<ReviewLedger>,
}

impl ReviewDispositionTool {
    pub fn new(ledger: Arc<ReviewLedger>) -> Self {
        Self { ledger }
    }
}

impl Tool for ReviewDispositionTool {
    fn name(&self) -> &'static str {
        "review_disposition"
    }

    fn description(&self) -> &'static str {
        "Record your decision on a review finding after acting on it: \
        \"fixed\" (code changed), \"disputed\" (contested; note with the \
        reason required), or \"no-action\" (uncontested, no change \
        warranted — an ignored nit or an answered question). finding_id \
        is the ledger id surfaced when the finding was recorded."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(DispositionArgs))
            .expect("schema serialization failed")
    }

    fn execute(
        &self,
        args: serde_json::Value,
        _ctx: ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let args: DispositionArgs = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
            if matches!(args.disposition, Disposition::Disputed)
                && args.note.as_deref().is_none_or(str::is_empty)
            {
                return Err(ToolError::InvalidArguments(
                    "a dispute requires a note with the reason".into(),
                ));
            }
            let found = self
                .ledger
                .set_disposition(
                    args.finding_id,
                    args.disposition.as_str(),
                    args.note.as_deref(),
                )
                .map_err(|e| ToolError::ExecutionFailed(format!("review_disposition: {e}")))?;
            if !found {
                return Err(ToolError::ExecutionFailed(format!(
                    "no finding with id {}",
                    args.finding_id
                )));
            }
            Ok("Disposition recorded.".to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: &str = r#"Prose review here.

```findings
{
  "verdict": "incorrect",
  "confidence": 0.9,
  "explanation": "one real bug",
  "findings": [
    {"category": "swallowed-error", "severity": "must-fix",
     "confidence": 0.8, "file": "src/x.rs", "line": 42,
     "note": "the Err arm drops the error"}
  ]
}
```"#;

    #[test]
    fn parses_full_block() {
        let out = parse_findings_block(BLOCK).unwrap();
        assert_eq!(out.verdict, Verdict::Incorrect);
        assert_eq!(out.confidence, Some(0.9));
        assert_eq!(out.findings.len(), 1);
        let f = &out.findings[0];
        assert_eq!(f.category, "swallowed-error");
        assert_eq!(f.severity.as_deref(), Some("must-fix"));
        assert_eq!(f.line, Some(42));
    }

    #[test]
    fn parses_clean_block() {
        let text = "All good.\n```findings\n{\"verdict\": \"correct\", \
                    \"confidence\": 1.0, \"explanation\": \"clean\", \
                    \"findings\": []}\n```";
        let out = parse_findings_block(text).unwrap();
        assert_eq!(out.verdict, Verdict::Correct);
        assert!(out.findings.is_empty());
    }

    #[test]
    fn missing_block_is_none() {
        assert!(parse_findings_block("no block here").is_none());
    }

    #[test]
    fn malformed_json_is_none() {
        assert!(parse_findings_block("```findings\nnot json\n```").is_none());
    }

    #[test]
    fn unterminated_block_is_none() {
        assert!(parse_findings_block("```findings\n{\"verdict\": \"correct\"}").is_none());
    }

    #[test]
    fn last_block_wins() {
        // A reviewer quoting an example block earlier in its prose must
        // not shadow the real trailer.
        let text = format!("```findings\n{{\"verdict\": \"correct\"}}\n```\n{BLOCK}");
        let out = parse_findings_block(&text).unwrap();
        assert_eq!(out.verdict, Verdict::Incorrect);
    }

    fn ledger() -> (tempfile::TempDir, ReviewLedger) {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ReviewLedger::open(&dir.path().join("review.db")).unwrap();
        (dir, ledger)
    }

    fn gate<'a>() -> GateRecord<'a> {
        GateRecord {
            repo: "owner/repo",
            gate: "commit",
            git_ref: "abc123",
        }
    }

    #[test]
    fn migration_adds_disposition_columns_to_legacy_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("review.db");
        // A ledger created before the disposition columns existed.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE reviews (
                     id INTEGER PRIMARY KEY,
                     recorded_at TEXT NOT NULL DEFAULT (datetime('now')),
                     repo TEXT NOT NULL, gate TEXT NOT NULL,
                     git_ref TEXT NOT NULL, verdict TEXT NOT NULL,
                     confidence REAL);
                 CREATE TABLE findings (
                     id INTEGER PRIMARY KEY,
                     recorded_at TEXT NOT NULL DEFAULT (datetime('now')),
                     repo TEXT NOT NULL, gate TEXT NOT NULL,
                     git_ref TEXT NOT NULL, source TEXT NOT NULL,
                     category TEXT NOT NULL, severity TEXT,
                     confidence REAL, file TEXT, line INTEGER,
                     note TEXT NOT NULL);
                 INSERT INTO findings (repo, gate, git_ref, source, category, note)
                 VALUES ('o/r', 'external', '1', 'human', 'x', 'pre-migration row');",
            )
            .unwrap();
        }

        let ledger = ReviewLedger::open(&path).unwrap();
        let conn = ledger.conn.lock().unwrap();
        let (rows, pending): (i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), SUM(disposition IS NULL) FROM findings",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((rows, pending), (1, 1), "old rows survive as pending");
    }

    #[test]
    fn reopen_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("review.db");
        drop(ReviewLedger::open(&path).unwrap());
        // Second open must not fail on already-added columns.
        ReviewLedger::open(&path).unwrap();
    }

    #[test]
    fn record_review_returns_finding_ids() {
        let (_dir, ledger) = ledger();
        let output = parse_findings_block(BLOCK).unwrap();
        let ids = ledger.record_review(&gate(), &output).unwrap();
        assert_eq!(ids.len(), output.findings.len());

        let conn = ledger.conn.lock().unwrap();
        let category: String = conn
            .query_row(
                "SELECT category FROM findings WHERE id = ?1",
                [ids[0]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(category, "swallowed-error");
    }

    #[test]
    fn record_finding_returns_id() {
        let (_dir, ledger) = ledger();
        let id = ledger
            .record_finding(&ExternalFinding {
                repo: "o/r",
                gate: "external",
                git_ref: "1",
                source: "human",
                category: "x",
                file: None,
                line: None,
                note: "n",
            })
            .unwrap();
        let conn = ledger.conn.lock().unwrap();
        let note: String = conn
            .query_row("SELECT note FROM findings WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(note, "n");
    }

    #[test]
    fn record_and_report_roundtrip() {
        let (_dir, ledger) = ledger();
        let output = parse_findings_block(BLOCK).unwrap();
        ledger.record_review(&gate(), &output).unwrap();

        let report = ledger.report().unwrap();
        assert!(report.contains("commit"), "{report}");
        assert!(report.contains("swallowed-error"), "{report}");
        assert!(report.contains("Incorrect"), "{report}");
    }

    #[test]
    fn empty_report_is_a_notice() {
        let (_dir, ledger) = ledger();
        assert_eq!(ledger.report().unwrap(), "No reviews recorded.");
    }

    #[tokio::test]
    async fn review_log_records_external_finding() {
        let (_dir, ledger) = ledger();
        let ledger = Arc::new(ledger);
        let tool = ReviewLogTool::new(ledger.clone());
        let reply = tool
            .execute(
                serde_json::json!({
                    "repo": "owner/repo",
                    "gate": "external",
                    "git_ref": "142",
                    "source": "human",
                    "category": "swallowed-error",
                    "file": "src/x.rs",
                    "line": 7,
                    "note": "reviewer caught a dropped Err"
                }),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        // The id is the handle a later disposition points at.
        assert_eq!(reply, "Recorded finding #1.");
        let report = ledger.report().unwrap();
        assert!(report.contains("swallowed-error"), "{report}");
        // The external column carries it, not self.
        let row: (i64, i64) = {
            let conn = ledger.conn.lock().unwrap();
            conn.query_row(
                "SELECT SUM(source = 'self'), SUM(source != 'self') FROM findings",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        assert_eq!(row, (0, 1));
    }

    #[tokio::test]
    async fn review_log_rejects_self_source() {
        let (_dir, ledger) = ledger();
        let tool = ReviewLogTool::new(Arc::new(ledger));
        let err = tool
            .execute(
                serde_json::json!({
                    "repo": "o/r", "gate": "external", "git_ref": "1",
                    "source": "self", "category": "x", "note": "n"
                }),
                ToolCtx::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[test]
    fn set_disposition_roundtrip() {
        let (_dir, ledger) = ledger();
        let output = parse_findings_block(BLOCK).unwrap();
        let ids = ledger.record_review(&gate(), &output).unwrap();

        assert!(ledger.set_disposition(ids[0], "fixed", None).unwrap());

        let conn = ledger.conn.lock().unwrap();
        let (disposition, disposed_at): (String, Option<String>) = conn
            .query_row(
                "SELECT disposition, disposed_at FROM findings WHERE id = ?1",
                [ids[0]],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(disposition, "fixed");
        assert!(disposed_at.is_some());
    }

    #[test]
    fn set_disposition_unknown_id_is_false() {
        let (_dir, ledger) = ledger();
        assert!(!ledger.set_disposition(999, "fixed", None).unwrap());
    }

    #[tokio::test]
    async fn disposition_tool_records_decision() {
        let (_dir, ledger) = ledger();
        let ledger = Arc::new(ledger);
        let id = ledger
            .record_finding(&ExternalFinding {
                repo: "o/r",
                gate: "external",
                git_ref: "1",
                source: "bot",
                category: "x",
                file: None,
                line: None,
                note: "n",
            })
            .unwrap();

        let tool = ReviewDispositionTool::new(ledger.clone());
        tool.execute(
            serde_json::json!({
                "finding_id": id,
                "disposition": "disputed",
                "note": "the guard is required; removing it breaks retries"
            }),
            ToolCtx::default(),
        )
        .await
        .unwrap();

        let conn = ledger.conn.lock().unwrap();
        let (disposition, note): (String, String) = conn
            .query_row(
                "SELECT disposition, disposition_note FROM findings WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(disposition, "disputed");
        assert!(note.contains("retries"));
    }

    #[tokio::test]
    async fn disposition_tool_rejects_dispute_without_note() {
        let (_dir, ledger) = ledger();
        let tool = ReviewDispositionTool::new(Arc::new(ledger));
        let err = tool
            .execute(
                serde_json::json!({"finding_id": 1, "disposition": "disputed"}),
                ToolCtx::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn disposition_tool_errors_on_unknown_id() {
        let (_dir, ledger) = ledger();
        let tool = ReviewDispositionTool::new(Arc::new(ledger));
        let err = tool
            .execute(
                serde_json::json!({"finding_id": 999, "disposition": "no-action"}),
                ToolCtx::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ExecutionFailed(_)));
    }

    #[test]
    fn report_splits_dispositions_by_source() {
        let (_dir, ledger) = ledger();
        // Two self findings: one fixed, one left pending.
        let output = ReviewOutput {
            verdict: Verdict::Incorrect,
            confidence: Some(0.9),
            explanation: "two problems".into(),
            findings: vec![
                Finding {
                    category: "swallowed-error".into(),
                    severity: Some("must-fix".into()),
                    confidence: None,
                    file: None,
                    line: None,
                    note: "a".into(),
                },
                Finding {
                    category: "comment-noise".into(),
                    severity: Some("nit".into()),
                    confidence: None,
                    file: None,
                    line: None,
                    note: "b".into(),
                },
            ],
        };
        let ids = ledger.record_review(&gate(), &output).unwrap();
        assert!(ledger.set_disposition(ids[0], "fixed", None).unwrap());
        // One external finding, disputed.
        let ext = ledger
            .record_finding(&ExternalFinding {
                repo: "o/r",
                gate: "external",
                git_ref: "9",
                source: "bot",
                category: "unneeded-guard",
                file: None,
                line: None,
                note: "c",
            })
            .unwrap();
        assert!(
            ledger
                .set_disposition(ext, "disputed", Some("the guard is load-bearing"))
                .unwrap()
        );

        let report = ledger.report().unwrap();
        assert!(report.contains("Dispositions by source"), "{report}");
        // self: 2 total, 1 fixed, 1 pending.
        assert!(
            report.contains("self          2      1         0          0        1"),
            "{report}"
        );
        // bot: 1 total, 1 disputed.
        assert!(
            report.contains("bot           1      0         1          0        0"),
            "{report}"
        );
    }

    #[test]
    fn gates_segment_names_the_contract() {
        assert!(GATES_SEGMENT.starts_with("## Review Gates"));
        for needle in [
            "\"plan\"",
            "\"commit\"",
            "\"series\"",
            "review_log",
            "review_disposition",
            "must-fix",
        ] {
            assert!(GATES_SEGMENT.contains(needle), "segment omits {needle}");
        }
    }

    #[test]
    fn clean_review_records_verdict_without_findings() {
        let (_dir, ledger) = ledger();
        let output = ReviewOutput {
            verdict: Verdict::Correct,
            confidence: Some(1.0),
            explanation: "clean".into(),
            findings: Vec::new(),
        };
        ledger.record_review(&gate(), &output).unwrap();
        let report = ledger.report().unwrap();
        assert!(report.contains("commit"), "{report}");
    }
}
