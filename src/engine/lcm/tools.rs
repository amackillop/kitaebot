//! Retrieval tools contributed by the LCM engine.
//!
//! Each tool holds its own read-only [`Connection`], opened from the
//! same database path the engine writes to. WAL lets multiple readers
//! coexist with one writer, so retrieval queries never block (or get
//! blocked by) the actor's writes.
//!
//! All three tools target the engine's currently active conversation.
//! That target follows session switches via a shared
//! [`AtomicI64`] held jointly by the engine and the tools. The atomic
//! is read once at the start of each call so a switch mid-call (which
//! cannot happen — the actor is single-threaded — but is permitted by
//! the type) does not split a single query across two conversations.
//!
//! ## Coverage of the spec at this stage
//!
//! Compaction has not yet landed (spec 14 §"Compaction" is in flight),
//! so `summaries`, `summary_messages`, `summary_parents`, and
//! `large_files` are all empty in practice. The tools still issue the
//! correct queries against those tables and return empty results
//! gracefully:
//!
//! - `lcm_grep` matches messages today; the summaries branch returns
//!   no rows but the SQL is in place.
//! - `lcm_describe` answers "no summary/file with that id" until the
//!   relevant tables are populated.
//! - `lcm_expand` walks the DAG once summaries exist; until then it
//!   reports the summary as missing.
//!
//! When 3.7 wires compaction in, these tools start returning real
//! data without further changes.

use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, params};
use schemars::JsonSchema;
use serde::Deserialize;
use tracing::debug;

use crate::error::ToolError;
use crate::tools::Tool;

/// Default result limit for `lcm_grep`. Generous but capped so a
/// pathological query cannot dump the whole conversation back to the
/// model.
const DEFAULT_GREP_LIMIT: u32 = 50;
const MAX_GREP_LIMIT: u32 = 200;

/// Conservative cap for `lcm_expand` until sub-agents land
/// (spec 19). The spec dictates 5000.
const DEFAULT_EXPAND_TOKEN_CAP: u32 = 5_000;
const MAX_EXPAND_TOKEN_CAP: u32 = 20_000;

const SNIPPET_CHARS: usize = 200;

fn exec_err(e: &rusqlite::Error) -> ToolError {
    ToolError::ExecutionFailed(format!("lcm: {e}"))
}

/// Wrap a sync DB closure on Tokio's blocking pool.
///
/// Mirrors the engine's `run_blocking` helper — every query the tools
/// make is a synchronous `rusqlite` call against `libsqlite3`, so it
/// has no business sitting on the executor thread.
fn run_blocking<F, T>(
    conn: Arc<Mutex<Connection>>,
    f: F,
) -> Pin<Box<dyn Future<Output = Result<T, ToolError>> + Send>>
where
    F: FnOnce(&Connection) -> Result<T, ToolError> + Send + 'static,
    T: Send + 'static,
{
    Box::pin(async move {
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().expect("LCM tool connection poisoned");
            f(&guard)
        })
        .await
        .map_err(|e| ToolError::ExecutionFailed(format!("blocking task failed: {e}")))?
    })
}

/// Truncate a snippet for grep results so a single match doesn't blow
/// out the response. Newlines collapse to spaces — the model gets one
/// line per hit, regardless of source formatting.
fn snippet(s: &str) -> String {
    let cleaned: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if cleaned.chars().count() <= SNIPPET_CHARS {
        cleaned
    } else {
        let cut = cleaned
            .char_indices()
            .nth(SNIPPET_CHARS)
            .map_or(cleaned.len(), |(i, _)| i);
        format!("{}...", &cleaned[..cut])
    }
}

// ── lcm_grep ────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
struct GrepArgs {
    /// Search pattern. FTS5 query syntax in `fts` mode (token search,
    /// boolean operators, phrase queries). Rust regex syntax in
    /// `regex` mode.
    pattern: String,
    /// `fts` (default) or `regex`.
    #[serde(default)]
    mode: Option<String>,
    /// `messages`, `summaries`, or `both` (default).
    #[serde(default)]
    scope: Option<String>,
    /// Max results returned. Defaults to 50; capped at 200.
    #[serde(default)]
    limit: Option<u32>,
}

/// Search compacted history.
pub struct LcmGrep {
    conn: Arc<Mutex<Connection>>,
    active_id: Arc<AtomicI64>,
}

impl LcmGrep {
    pub fn new(conn: Connection, active_id: Arc<AtomicI64>) -> Self {
        Self {
            conn: Arc::new(Mutex::new(conn)),
            active_id,
        }
    }
}

impl Tool for LcmGrep {
    fn name(&self) -> &'static str {
        "lcm_grep"
    }

    fn description(&self) -> &'static str {
        "Search the active session's compacted history for keywords or patterns. \
         Use mode=fts (default, token-based search with boolean operators) for keywords; \
         mode=regex for arbitrary patterns. Scope filters to messages, summaries, or both. \
         Returns IDs you can pass to lcm_describe or lcm_expand."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(GrepArgs)).expect("lcm_grep schema")
    }

    fn execute(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        let conn = Arc::clone(&self.conn);
        let conversation_id = self.active_id.load(Ordering::Acquire);
        Box::pin(async move {
            let args: GrepArgs = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
            let mode = args.mode.as_deref().unwrap_or("fts").to_string();
            let scope = args.scope.as_deref().unwrap_or("both").to_string();
            let limit = args.limit.unwrap_or(DEFAULT_GREP_LIMIT).min(MAX_GREP_LIMIT);

            match mode.as_str() {
                "fts" | "regex" => {}
                other => {
                    return Err(ToolError::InvalidArguments(format!(
                        "mode must be \"fts\" or \"regex\", got {other:?}"
                    )));
                }
            }
            match scope.as_str() {
                "messages" | "summaries" | "both" => {}
                other => {
                    return Err(ToolError::InvalidArguments(format!(
                        "scope must be \"messages\", \"summaries\", or \"both\", got {other:?}"
                    )));
                }
            }

            debug!(pattern = %args.pattern, mode, scope, limit, "lcm_grep");
            run_blocking(conn, move |c| {
                run_grep(c, conversation_id, &args.pattern, &mode, &scope, limit)
            })
            .await
        })
    }
}

fn run_grep(
    conn: &Connection,
    conversation_id: i64,
    pattern: &str,
    mode: &str,
    scope: &str,
    limit: u32,
) -> Result<String, ToolError> {
    let mut hits: Vec<String> = Vec::new();
    let lim = i64::from(limit);

    if matches!(scope, "messages" | "both") {
        hits.extend(grep_messages(conn, conversation_id, pattern, mode, lim)?);
    }
    if matches!(scope, "summaries" | "both") {
        hits.extend(grep_summaries(conn, conversation_id, pattern, mode, lim)?);
    }

    if hits.is_empty() {
        Ok(format!("No matches for {pattern:?} in {scope} ({mode})."))
    } else {
        Ok(format!("{} match(es):\n{}", hits.len(), hits.join("\n")))
    }
}

fn grep_messages(
    conn: &Connection,
    conversation_id: i64,
    pattern: &str,
    mode: &str,
    limit: i64,
) -> Result<Vec<String>, ToolError> {
    let sql = match mode {
        "fts" => {
            "SELECT m.message_id, m.role, m.content \
                  FROM messages_fts \
                  JOIN messages m ON m.message_id = messages_fts.rowid \
                  WHERE messages_fts MATCH ?1 AND m.conversation_id = ?2 \
                  LIMIT ?3"
        }
        "regex" => {
            "SELECT message_id, role, content \
                    FROM messages \
                    WHERE conversation_id = ?2 AND content REGEXP ?1 \
                    LIMIT ?3"
        }
        _ => unreachable!("mode validated upstream"),
    };
    let mut stmt = conn.prepare(sql).map_err(|e| exec_err(&e))?;
    let rows = stmt
        .query_map(params![pattern, conversation_id, limit], |r| {
            let id: i64 = r.get(0)?;
            let role: String = r.get(1)?;
            let content: String = r.get(2)?;
            Ok(format!(
                "[message_id={id} role={role}] {}",
                snippet(&content)
            ))
        })
        .map_err(|e| exec_err(&e))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| exec_err(&e))
}

fn grep_summaries(
    conn: &Connection,
    conversation_id: i64,
    pattern: &str,
    mode: &str,
    limit: i64,
) -> Result<Vec<String>, ToolError> {
    let sql = match mode {
        "fts" => {
            "SELECT s.summary_id, s.kind, s.depth, s.content \
                  FROM summaries_fts \
                  JOIN summaries s ON s.summary_id = summaries_fts.summary_id \
                  WHERE summaries_fts MATCH ?1 AND s.conversation_id = ?2 \
                  LIMIT ?3"
        }
        "regex" => {
            "SELECT summary_id, kind, depth, content \
                    FROM summaries \
                    WHERE conversation_id = ?2 AND content REGEXP ?1 \
                    LIMIT ?3"
        }
        _ => unreachable!("mode validated upstream"),
    };
    let mut stmt = conn.prepare(sql).map_err(|e| exec_err(&e))?;
    let rows = stmt
        .query_map(params![pattern, conversation_id, limit], |r| {
            let id: String = r.get(0)?;
            let kind: String = r.get(1)?;
            let depth: i64 = r.get(2)?;
            let content: String = r.get(3)?;
            Ok(format!(
                "[summary_id={id} kind={kind} depth={depth}] {}",
                snippet(&content)
            ))
        })
        .map_err(|e| exec_err(&e))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| exec_err(&e))
}

// ── lcm_describe ────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
struct DescribeArgs {
    /// Summary ID (`sum_xxx`) or file ID (`file_xxx`). The prefix
    /// determines which table is consulted.
    id: String,
}

/// Inspect a summary or file node by ID.
pub struct LcmDescribe {
    conn: Arc<Mutex<Connection>>,
    active_id: Arc<AtomicI64>,
}

impl LcmDescribe {
    pub fn new(conn: Connection, active_id: Arc<AtomicI64>) -> Self {
        Self {
            conn: Arc::new(Mutex::new(conn)),
            active_id,
        }
    }
}

impl Tool for LcmDescribe {
    fn name(&self) -> &'static str {
        "lcm_describe"
    }

    fn description(&self) -> &'static str {
        "Inspect a summary or large-file node from the active session by ID. \
         Returns metadata (depth, time range, descendant count, parents, source IDs) \
         for sum_* IDs, or path/size/exploration summary for file_* IDs."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(DescribeArgs)).expect("lcm_describe schema")
    }

    fn execute(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        let conn = Arc::clone(&self.conn);
        let conversation_id = self.active_id.load(Ordering::Acquire);
        Box::pin(async move {
            let args: DescribeArgs = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
            let id = args.id.clone();
            run_blocking(conn, move |c| {
                if id.starts_with("sum_") {
                    describe_summary(c, conversation_id, &id)
                } else if id.starts_with("file_") {
                    Ok(describe_file(c, conversation_id, &id))
                } else {
                    Err(ToolError::InvalidArguments(format!(
                        "id must start with \"sum_\" or \"file_\", got {id:?}"
                    )))
                }
            })
            .await
        })
    }
}

#[allow(clippy::too_many_lines)]
fn describe_summary(
    conn: &Connection,
    conversation_id: i64,
    summary_id: &str,
) -> Result<String, ToolError> {
    let row = conn
        .query_row(
            "SELECT kind, depth, content, token_count, earliest_at, latest_at, \
                    descendant_count, descendant_token_count, model, created_at \
             FROM summaries \
             WHERE summary_id = ?1 AND conversation_id = ?2",
            params![summary_id, conversation_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, i64>(7)?,
                    r.get::<_, String>(8)?,
                    r.get::<_, String>(9)?,
                ))
            },
        )
        .ok();

    let Some((
        kind,
        depth,
        content,
        token_count,
        earliest_at,
        latest_at,
        descendant_count,
        descendant_token_count,
        model,
        created_at,
    )) = row
    else {
        return Ok(format!(
            "No summary with id {summary_id:?} in the active session."
        ));
    };

    let parents = collect_strings(
        conn,
        "SELECT parent_summary_id FROM summary_parents WHERE summary_id = ?1 ORDER BY parent_summary_id",
        params![summary_id],
    )?;
    let children = collect_strings(
        conn,
        "SELECT summary_id FROM summary_parents WHERE parent_summary_id = ?1 ORDER BY summary_id",
        params![summary_id],
    )?;
    let source_messages = collect_i64s(
        conn,
        "SELECT message_id FROM summary_messages WHERE summary_id = ?1 ORDER BY message_id",
        params![summary_id],
    )?;
    let files = collect_strings(
        conn,
        "SELECT file_id FROM summary_files WHERE summary_id = ?1 ORDER BY file_id",
        params![summary_id],
    )?;

    Ok(format!(
        "summary_id: {summary_id}\n\
         kind: {kind}\n\
         depth: {depth}\n\
         token_count: {token_count}\n\
         earliest_at: {earliest_at}\n\
         latest_at: {latest_at}\n\
         descendants: {descendant_count} ({descendant_token_count} tokens)\n\
         model: {model}\n\
         created_at: {created_at}\n\
         parents: {}\n\
         children: {}\n\
         source_messages: {}\n\
         files: {}\n\n\
         content:\n{}",
        if parents.is_empty() {
            "(none)".into()
        } else {
            parents.join(", ")
        },
        if children.is_empty() {
            "(none)".into()
        } else {
            children.join(", ")
        },
        if source_messages.is_empty() {
            "(none)".into()
        } else {
            source_messages
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        },
        if files.is_empty() {
            "(none)".into()
        } else {
            files.join(", ")
        },
        content,
    ))
}

fn describe_file(conn: &Connection, conversation_id: i64, file_id: &str) -> String {
    let row = conn
        .query_row(
            "SELECT path, mime_type, byte_size, token_count, exploration_summary, \
                    first_seen_message_id, created_at \
             FROM large_files \
             WHERE file_id = ?1 AND conversation_id = ?2",
            params![file_id, conversation_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                    r.get::<_, String>(6)?,
                ))
            },
        )
        .ok();

    let Some((path, mime_type, byte_size, token_count, exploration, first_seen, created_at)) = row
    else {
        return format!("No file with id {file_id:?} in the active session.");
    };

    format!(
        "file_id: {file_id}\n\
         path: {path}\n\
         mime_type: {mime_type}\n\
         byte_size: {byte_size}\n\
         token_count: {token_count}\n\
         first_seen_message_id: {}\n\
         created_at: {created_at}\n\n\
         exploration_summary:\n{exploration}",
        first_seen.map_or_else(|| "(unknown)".into(), |id| id.to_string()),
    )
}

fn collect_strings(
    conn: &Connection,
    sql: &str,
    p: impl rusqlite::Params,
) -> Result<Vec<String>, ToolError> {
    let mut stmt = conn.prepare(sql).map_err(|e| exec_err(&e))?;
    let rows = stmt
        .query_map(p, |r| r.get::<_, String>(0))
        .map_err(|e| exec_err(&e))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| exec_err(&e))
}

fn collect_i64s(
    conn: &Connection,
    sql: &str,
    p: impl rusqlite::Params,
) -> Result<Vec<i64>, ToolError> {
    let mut stmt = conn.prepare(sql).map_err(|e| exec_err(&e))?;
    let rows = stmt
        .query_map(p, |r| r.get::<_, i64>(0))
        .map_err(|e| exec_err(&e))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| exec_err(&e))
}

// ── lcm_expand ──────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
struct ExpandArgs {
    /// Summary to expand.
    summary_id: String,
    /// Levels to expand (default 1). Each level fetches one DAG step
    /// further from `summary_id`.
    #[serde(default)]
    depth: Option<u32>,
    /// Include raw source messages for leaf summaries (default false).
    #[serde(default)]
    include_messages: Option<bool>,
    /// Token cap on response. Default 5000; capped at 20000. Stops
    /// expansion early once the cap is reached.
    #[serde(default)]
    token_cap: Option<u32>,
}

/// Drill into a summary, returning child summaries (and optionally
/// source messages for leaves) up to `depth` levels and `token_cap`
/// tokens. Restricted to sub-agents per spec 14, but until spec 19
/// lands the main agent has access with a conservative cap.
pub struct LcmExpand {
    conn: Arc<Mutex<Connection>>,
    active_id: Arc<AtomicI64>,
}

impl LcmExpand {
    pub fn new(conn: Connection, active_id: Arc<AtomicI64>) -> Self {
        Self {
            conn: Arc::new(Mutex::new(conn)),
            active_id,
        }
    }
}

impl Tool for LcmExpand {
    fn name(&self) -> &'static str {
        "lcm_expand"
    }

    fn description(&self) -> &'static str {
        "Drill into a summary node. For condensed summaries, returns child summaries; \
         for leaf summaries, optionally returns the raw source messages. \
         Stops once token_cap is reached. Use sparingly: expanding deep summaries can \
         recover large volumes of conversation."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(ExpandArgs)).expect("lcm_expand schema")
    }

    fn execute(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        let conn = Arc::clone(&self.conn);
        let conversation_id = self.active_id.load(Ordering::Acquire);
        Box::pin(async move {
            let args: ExpandArgs = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
            let depth = args.depth.unwrap_or(1).max(1);
            let include_messages = args.include_messages.unwrap_or(false);
            let token_cap = args
                .token_cap
                .unwrap_or(DEFAULT_EXPAND_TOKEN_CAP)
                .min(MAX_EXPAND_TOKEN_CAP);
            let summary_id = args.summary_id.clone();
            run_blocking(conn, move |c| {
                expand(
                    c,
                    conversation_id,
                    &summary_id,
                    depth,
                    include_messages,
                    token_cap,
                )
            })
            .await
        })
    }
}

#[allow(clippy::too_many_lines)]
fn expand(
    conn: &Connection,
    conversation_id: i64,
    summary_id: &str,
    depth: u32,
    include_messages: bool,
    token_cap: u32,
) -> Result<String, ToolError> {
    // Confirm the root summary exists in this conversation. If it
    // does not — the only path users hit today, since compaction is
    // not yet wired — report cleanly rather than walking the DAG.
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM summaries \
             WHERE summary_id = ?1 AND conversation_id = ?2",
            params![summary_id, conversation_id],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if !exists {
        return Ok(format!(
            "No summary with id {summary_id:?} in the active session."
        ));
    }

    let mut out = String::new();
    let mut tokens_used: u32 = 0;
    let cap = token_cap as usize;

    let mut frontier: Vec<(String, u32)> = vec![(summary_id.to_string(), 0)];
    let mut seen: std::collections::HashSet<String> =
        std::collections::HashSet::from([summary_id.to_string()]);

    while let Some((id, level)) = frontier.pop() {
        let info = conn
            .query_row(
                "SELECT kind, depth, content, token_count \
                 FROM summaries WHERE summary_id = ?1",
                params![id.as_str()],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                },
            )
            .map_err(|e| exec_err(&e))?;
        let (kind, node_depth, content, token_count) = info;

        let chunk =
            format!("## {id} (kind={kind}, depth={node_depth}, level={level})\n{content}\n\n");
        if tokens_used as usize + chunk.len() / 4 > cap {
            let _ = writeln!(
                out,
                "[truncated at token_cap={cap}; remaining frontier: {} node(s)]",
                frontier.len() + 1
            );
            return Ok(out);
        }
        out.push_str(&chunk);
        tokens_used += u32::try_from(token_count).unwrap_or(u32::MAX);

        if level >= depth {
            continue;
        }

        if kind == "leaf" {
            if include_messages {
                let mut stmt = conn
                    .prepare(
                        "SELECT m.message_id, m.role, m.content \
                         FROM summary_messages sm \
                         JOIN messages m ON m.message_id = sm.message_id \
                         WHERE sm.summary_id = ?1 \
                         ORDER BY m.seq",
                    )
                    .map_err(|e| exec_err(&e))?;
                let rows = stmt
                    .query_map(params![id.as_str()], |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    })
                    .map_err(|e| exec_err(&e))?;
                for r in rows {
                    let (mid, role, mc) = r.map_err(|e| exec_err(&e))?;
                    let block = format!("### message_id={mid} role={role}\n{mc}\n\n");
                    if tokens_used as usize + block.len() / 4 > cap {
                        let _ = writeln!(out, "[truncated at token_cap={cap}]");
                        return Ok(out);
                    }
                    out.push_str(&block);
                    tokens_used += u32::try_from(mc.len() / 4).unwrap_or(u32::MAX);
                }
            }
        } else {
            // Condensed — descend into children.
            let mut stmt = conn
                .prepare(
                    "SELECT summary_id FROM summary_parents \
                     WHERE parent_summary_id = ?1 \
                     ORDER BY summary_id",
                )
                .map_err(|e| exec_err(&e))?;
            let rows = stmt
                .query_map(params![id.as_str()], |r| r.get::<_, String>(0))
                .map_err(|e| exec_err(&e))?;
            for r in rows {
                let child = r.map_err(|e| exec_err(&e))?;
                if seen.insert(child.clone()) {
                    frontier.push((child, level + 1));
                }
            }
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::lcm::schema;
    use crate::tools::Tool;

    fn fresh_db() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lcm.db");
        let writer = schema::open(&db).unwrap();
        // Seed a conversation for tests to target.
        writer
            .execute(
                "INSERT INTO conversations(name, created_at, updated_at) \
                 VALUES ('general', '2025-01-01', '2025-01-01')",
                [],
            )
            .unwrap();
        (dir, db)
    }

    fn insert_message(conn: &Connection, conversation_id: i64, role: &str, content: &str) -> i64 {
        let seq: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(seq), -1) + 1 FROM messages WHERE conversation_id = ?1",
                [conversation_id],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO messages(conversation_id, seq, role, content, token_count, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, '2025-01-01')",
            params![
                conversation_id,
                seq,
                role,
                content,
                i64::try_from(content.len() / 4).unwrap_or(0),
            ],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn shared_active(id: i64) -> Arc<AtomicI64> {
        Arc::new(AtomicI64::new(id))
    }

    #[tokio::test]
    async fn lcm_grep_fts_finds_message_in_active_conversation() {
        let (_dir, db) = fresh_db();
        let writer = schema::open(&db).unwrap();
        insert_message(&writer, 1, "user", "the quick brown fox");
        insert_message(&writer, 1, "assistant", "lazy dog response");

        let tool = LcmGrep::new(schema::open_readonly(&db).unwrap(), shared_active(1));
        let out = tool
            .execute(serde_json::json!({"pattern": "fox"}))
            .await
            .unwrap();
        assert!(out.contains("brown fox"), "missing match in: {out}");
        assert!(out.contains("message_id="), "missing id in: {out}");
        assert!(!out.contains("lazy dog"), "unrelated row leaked: {out}");
    }

    #[tokio::test]
    async fn lcm_grep_regex_mode_messages() {
        let (_dir, db) = fresh_db();
        let writer = schema::open(&db).unwrap();
        insert_message(&writer, 1, "user", "phone: 555-1234");
        insert_message(&writer, 1, "user", "alpha bravo charlie");

        let tool = LcmGrep::new(schema::open_readonly(&db).unwrap(), shared_active(1));
        let out = tool
            .execute(serde_json::json!({
                "pattern": r"\d{3}-\d{4}",
                "mode": "regex",
                "scope": "messages",
            }))
            .await
            .unwrap();
        assert!(out.contains("555-1234"));
        assert!(!out.contains("bravo"));
    }

    #[tokio::test]
    async fn lcm_grep_no_matches_message_is_friendly() {
        let (_dir, db) = fresh_db();
        let tool = LcmGrep::new(schema::open_readonly(&db).unwrap(), shared_active(1));
        let out = tool
            .execute(serde_json::json!({"pattern": "nothingmatches"}))
            .await
            .unwrap();
        assert!(out.starts_with("No matches"));
    }

    #[tokio::test]
    async fn lcm_grep_rejects_unknown_mode() {
        let (_dir, db) = fresh_db();
        let tool = LcmGrep::new(schema::open_readonly(&db).unwrap(), shared_active(1));
        let err = tool
            .execute(serde_json::json!({"pattern": "x", "mode": "fuzzy"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn lcm_grep_isolates_conversation() {
        let (_dir, db) = fresh_db();
        let writer = schema::open(&db).unwrap();
        writer
            .execute(
                "INSERT INTO conversations(name, created_at, updated_at) \
                 VALUES ('other', '2025-01-01', '2025-01-01')",
                [],
            )
            .unwrap();
        insert_message(&writer, 1, "user", "general realm");
        insert_message(&writer, 2, "user", "other realm");

        // Active id points at conversation 2 → only "other realm" hits.
        let tool = LcmGrep::new(schema::open_readonly(&db).unwrap(), shared_active(2));
        let out = tool
            .execute(serde_json::json!({"pattern": "realm"}))
            .await
            .unwrap();
        assert!(out.contains("other realm"));
        assert!(!out.contains("general realm"));
    }

    #[tokio::test]
    async fn lcm_describe_unknown_summary_id() {
        let (_dir, db) = fresh_db();
        let tool = LcmDescribe::new(schema::open_readonly(&db).unwrap(), shared_active(1));
        let out = tool
            .execute(serde_json::json!({"id": "sum_missing"}))
            .await
            .unwrap();
        assert!(out.contains("No summary"));
    }

    #[tokio::test]
    async fn lcm_describe_unknown_file_id() {
        let (_dir, db) = fresh_db();
        let tool = LcmDescribe::new(schema::open_readonly(&db).unwrap(), shared_active(1));
        let out = tool
            .execute(serde_json::json!({"id": "file_missing"}))
            .await
            .unwrap();
        assert!(out.contains("No file"));
    }

    #[tokio::test]
    async fn lcm_describe_rejects_bad_prefix() {
        let (_dir, db) = fresh_db();
        let tool = LcmDescribe::new(schema::open_readonly(&db).unwrap(), shared_active(1));
        let err = tool
            .execute(serde_json::json!({"id": "garbage"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn lcm_expand_unknown_summary() {
        let (_dir, db) = fresh_db();
        let tool = LcmExpand::new(schema::open_readonly(&db).unwrap(), shared_active(1));
        let out = tool
            .execute(serde_json::json!({"summary_id": "sum_missing"}))
            .await
            .unwrap();
        assert!(out.contains("No summary"));
    }

    #[tokio::test]
    async fn lcm_expand_returns_existing_leaf_content() {
        let (_dir, db) = fresh_db();
        let writer = schema::open(&db).unwrap();
        let mid = insert_message(&writer, 1, "user", "raw raw raw");
        writer.execute(
            "INSERT INTO summaries(summary_id, conversation_id, kind, depth, content, token_count, \
                                   earliest_at, latest_at, descendant_count, descendant_token_count, \
                                   source_message_token_count, model, created_at) \
             VALUES ('sum_test', 1, 'leaf', 0, 'leaf summary text', 4, \
                     '2025-01-01', '2025-01-01', 1, 4, 4, 'mock', '2025-01-01')",
            [],
        )
        .unwrap();
        writer
            .execute(
                "INSERT INTO summary_messages(summary_id, message_id) VALUES ('sum_test', ?1)",
                [mid],
            )
            .unwrap();

        let tool = LcmExpand::new(schema::open_readonly(&db).unwrap(), shared_active(1));
        let out = tool
            .execute(serde_json::json!({
                "summary_id": "sum_test",
                "include_messages": true,
            }))
            .await
            .unwrap();
        assert!(out.contains("leaf summary text"), "out was: {out}");
        assert!(out.contains("raw raw raw"), "missing raw msg: {out}");
    }

    #[test]
    fn snippet_truncates_long_input() {
        let s = "a".repeat(SNIPPET_CHARS + 50);
        let out = snippet(&s);
        assert!(out.ends_with("..."));
        assert!(out.chars().count() <= SNIPPET_CHARS + 3);
    }

    #[test]
    fn snippet_collapses_newlines() {
        assert_eq!(snippet("a\nb\nc"), "a b c");
    }
}
