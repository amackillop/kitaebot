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
                 note        TEXT NOT NULL
             );",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Record one gate invocation: the review row plus one finding row
    /// per entry, all `source = 'self'`. One transaction; a gate either
    /// lands whole or not at all.
    pub fn record_review(&self, gate: &GateRecord, output: &ReviewOutput) -> rusqlite::Result<()> {
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
            }
        }
        tx.commit()
    }

    /// Record one externally-sourced finding (a human or review-bot
    /// comment). Severity and confidence stay NULL: those are
    /// self-review signals.
    pub fn record_finding(&self, f: &ExternalFinding) -> rusqlite::Result<()> {
        let conn = self.conn.lock().expect("review ledger mutex poisoned");
        conn.execute(
            "INSERT INTO findings
                 (repo, gate, git_ref, source, category, file, line, note)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                f.repo, f.gate, f.git_ref, f.source, f.category, f.file, f.line, f.note,
            ],
        )?;
        Ok(())
    }

    /// Render the `/findings` report: reviews by gate and verdict,
    /// then finding counts by category split self vs external.
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
        Ok(out)
    }
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
            self.ledger
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
            Ok("Recorded.".to_string())
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
        tool.execute(
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
