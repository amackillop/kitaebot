//! [`LcmEngine`] — `SQLite`-backed LCM context engine.
//!
//! Every push persists a row in `messages` (decomposed into
//! `message_parts`) and appends a `message`-kind item to
//! `context_items`. `assemble` walks `context_items` in order and
//! rehydrates each row back into a `Message` from `messages` + parts.
//! The DAG plumbing (`summaries`, `summary_*`, `large_files`) exists
//! in the schema but is not exercised yet — compaction comes later,
//! and `compact_if_needed` / `force_compact` currently return errors.
//! `'summary'` rows in `context_items` are likewise unreachable until
//! compaction lands; `assemble` skips them defensively.
//!
//! Active session persistence reuses `memory/active_session` — the
//! same plain-text file flat sessions write to, so switching engines
//! preserves the user's last session.
//!
//! Names are sanitized identically to flat sessions (`/` -> `--`)
//! because GitHub channel routing produces `owner/repo` strings; the
//! sanitization keeps them as legal `conversations.name` values.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, params};
use tracing::error;

use crate::config::ContextConfig;
use crate::error::EngineError;
use crate::tools::Tool;
use crate::types::{Message, ToolCall, ToolFunction};

use super::super::{
    AssembledContext, CompactionEvent, ContextEngine, ContextStats, SessionInfo, SummarizeFn,
};
use super::schema;
use super::tools::{LcmDescribe, LcmExpand, LcmGrep};

/// The connection lives behind `Arc<Mutex<_>>` for two reasons:
///
/// 1. `rusqlite::Connection` is `!Sync`, but [`ContextEngine`]
///    requires `Sync` so the actor task can hold an `&engine` across
///    `.await` points. `Mutex<Connection>` is `Sync`.
/// 2. Every async DB call moves the work onto Tokio's blocking pool
///    via [`spawn_blocking`](tokio::task::spawn_blocking). That
///    closure must be `'static`, so we clone the `Arc` into it
///    rather than borrowing `&self`. `SQLite` is genuinely blocking;
///    a multi-row transaction would otherwise stall the executor
///    thread for the duration.
///
/// Contention on the mutex is near-zero: there is at most one async
/// task per engine, and it always awaits the blocking task before
/// issuing the next call.
pub struct LcmEngine {
    conn: Arc<Mutex<Connection>>,
    db_path: PathBuf,
    active_name: String,
    conversation_id: i64,
    /// Shared with retrieval tools so they can target the current
    /// session without holding a reference to the engine. Updated
    /// atomically on every successful [`switch_session`] call.
    active_id: Arc<AtomicI64>,
    memory_dir: PathBuf,
    ctx: ContextConfig,
}

impl LcmEngine {
    /// Open or create the LCM database at `db_path`.
    ///
    /// Restores the active session from `memory/active_session` (or
    /// falls back to `"general"`), ensuring a `conversations` row
    /// exists for it.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Storage`] if the database cannot be
    /// opened or the active conversation row cannot be created.
    pub fn new(
        db_path: &Path,
        memory_dir: PathBuf,
        ctx: ContextConfig,
    ) -> Result<Self, EngineError> {
        let conn = schema::open(db_path)?;
        let active_name = read_active_session(&memory_dir).unwrap_or_else(|| "general".into());
        let conversation_id = ensure_conversation(&conn, &active_name)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            db_path: db_path.to_path_buf(),
            active_name,
            conversation_id,
            active_id: Arc::new(AtomicI64::new(conversation_id)),
            memory_dir,
            ctx,
        })
    }

    fn budget(&self) -> usize {
        self.ctx.max_tokens as usize * usize::from(self.ctx.budget_percent) / 100
    }

    /// Count and summed token estimate of items in the active context.
    ///
    /// Joins `context_items` against both `messages` and `summaries`
    /// so the same query keeps working once compaction starts emitting
    /// summary items.
    ///
    /// Synchronous because [`ContextEngine::stats`] is. A single
    /// `COUNT` under WAL is sub-millisecond; the `spawn_blocking`
    /// overhead would dominate.
    fn context_stats_query(&self) -> rusqlite::Result<(i64, i64)> {
        let conn = self.conn.lock().expect("LCM connection mutex poisoned");
        conn.query_row(
            "SELECT COUNT(*), \
                    COALESCE(SUM(m.token_count), 0) + COALESCE(SUM(s.token_count), 0) \
             FROM context_items ci \
             LEFT JOIN messages  m ON ci.message_id = m.message_id \
             LEFT JOIN summaries s ON ci.summary_id = s.summary_id \
             WHERE ci.conversation_id = ?1",
            [self.conversation_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
    }
}

impl ContextEngine for LcmEngine {
    async fn push_message(&mut self, msg: Message) -> Result<(), EngineError> {
        let conversation_id = self.conversation_id;
        let conn = Arc::clone(&self.conn);
        run_blocking(conn, move |c| push_message_sync(c, conversation_id, &msg)).await
    }

    async fn assemble(&self, system_prompt: &str) -> Result<AssembledContext, EngineError> {
        let conversation_id = self.conversation_id;
        let conn = Arc::clone(&self.conn);
        let system_prompt = system_prompt.to_string();
        run_blocking(conn, move |c| {
            assemble_sync(c, conversation_id, &system_prompt)
        })
        .await
    }

    async fn compact_if_needed(
        &mut self,
        _summarize: &SummarizeFn,
    ) -> Result<Option<CompactionEvent>, EngineError> {
        Err(EngineError::Storage(
            "lcm compact_if_needed: not implemented".into(),
        ))
    }

    async fn force_compact(
        &mut self,
        _summarize: &SummarizeFn,
    ) -> Result<CompactionEvent, EngineError> {
        Err(EngineError::Storage(
            "lcm force_compact: not implemented".into(),
        ))
    }

    async fn clear(&mut self) -> Result<(), EngineError> {
        // Drop the active context only. Raw messages and any summaries
        // stay in the store — that is the whole point of LCM. Recall
        // tools can still surface them after a clear.
        let conversation_id = self.conversation_id;
        let conn = Arc::clone(&self.conn);
        run_blocking(conn, move |c| {
            c.execute(
                "DELETE FROM context_items WHERE conversation_id = ?1",
                [conversation_id],
            )
            .map_err(|e| storage_err(&e))?;
            Ok(())
        })
        .await
    }

    async fn save(&mut self) -> Result<(), EngineError> {
        // No-op. Every push commits in its own transaction; WAL gives
        // us crash safety without an explicit save.
        Ok(())
    }

    fn stats(&self) -> ContextStats {
        let (count, tokens) = self.context_stats_query().unwrap_or((0, 0));
        ContextStats {
            message_count: usize::try_from(count).unwrap_or(0),
            token_estimate: usize::try_from(tokens).unwrap_or(0),
            budget: self.budget(),
        }
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        // Open three independent read-only connections — one per tool.
        // WAL lets these readers run concurrently with the engine's
        // writer. If a connection fails to open, log and skip that
        // tool: a missing retrieval tool degrades gracefully (the
        // model still has the active context), whereas panicking here
        // would take down the daemon for a non-essential feature.
        let mut tools: Vec<Box<dyn Tool>> = Vec::new();
        let open = |label: &'static str| -> Option<Connection> {
            schema::open_readonly(&self.db_path)
                .map_err(|e| error!(tool = label, "failed to open LCM tool connection: {e}"))
                .ok()
        };
        if let Some(conn) = open("lcm_grep") {
            tools.push(Box::new(LcmGrep::new(conn, Arc::clone(&self.active_id))));
        }
        if let Some(conn) = open("lcm_describe") {
            tools.push(Box::new(LcmDescribe::new(
                conn,
                Arc::clone(&self.active_id),
            )));
        }
        if let Some(conn) = open("lcm_expand") {
            tools.push(Box::new(LcmExpand::new(conn, Arc::clone(&self.active_id))));
        }
        tools
    }

    fn active_session(&self) -> &str {
        &self.active_name
    }

    async fn switch_session(&mut self, name: &str) -> Result<(), EngineError> {
        let sanitized = sanitize_name(name);
        if sanitized == self.active_name {
            return Ok(());
        }
        let conn = Arc::clone(&self.conn);
        let name_for_db = sanitized.clone();
        let id = run_blocking(conn, move |c| ensure_conversation(c, &name_for_db)).await?;
        self.active_name = sanitized;
        self.conversation_id = id;
        self.active_id.store(id, Ordering::Release);
        persist_active_session(&self.memory_dir, &self.active_name);
        Ok(())
    }

    async fn list_sessions(&self) -> Result<Vec<SessionInfo>, EngineError> {
        let conn = Arc::clone(&self.conn);
        run_blocking(conn, list_sessions_sync).await
    }
}

/// Run a blocking DB closure on Tokio's blocking pool.
///
/// Every async [`ContextEngine`] method that touches `SQLite` funnels
/// through here. The closure receives `&mut Connection` (locked from
/// the shared mutex) and returns a `Result<T, EngineError>`. A
/// `JoinError` from `spawn_blocking` is reported as `Storage`.
async fn run_blocking<F, T>(conn: Arc<Mutex<Connection>>, f: F) -> Result<T, EngineError>
where
    F: FnOnce(&mut Connection) -> Result<T, EngineError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let mut guard = conn.lock().expect("LCM connection mutex poisoned");
        f(&mut guard)
    })
    .await
    .map_err(|e| EngineError::Storage(format!("blocking task failed: {e}")))?
}

// ── Internal helpers ────────────────────────────────────────────────

/// Persist `msg` into `messages` + `message_parts` and append a
/// `'message'` row to `context_items`. Wrapped in a single transaction
/// so a partial failure cannot leave a half-decomposed message.
fn push_message_sync(
    conn: &mut Connection,
    conversation_id: i64,
    msg: &Message,
) -> Result<(), EngineError> {
    let role = role_str(msg);
    let content = msg.content().to_string();
    let token_count = i64::try_from(msg.char_count() / 4).unwrap_or(i64::MAX);

    let tx = conn.transaction().map_err(|e| storage_err(&e))?;

    let seq: i64 = tx
        .query_row(
            "SELECT COALESCE(MAX(seq), -1) + 1 FROM messages \
             WHERE conversation_id = ?1",
            [conversation_id],
            |row| row.get(0),
        )
        .map_err(|e| storage_err(&e))?;

    tx.execute(
        "INSERT INTO messages \
             (conversation_id, seq, role, content, token_count, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
        params![conversation_id, seq, role, content, token_count],
    )
    .map_err(|e| storage_err(&e))?;
    let message_id = tx.last_insert_rowid();

    insert_parts(&tx, message_id, msg)?;

    let next_ord: i64 = tx
        .query_row(
            "SELECT COALESCE(MAX(ordinal), -1) + 1 FROM context_items \
             WHERE conversation_id = ?1",
            [conversation_id],
            |row| row.get(0),
        )
        .map_err(|e| storage_err(&e))?;
    tx.execute(
        "INSERT INTO context_items \
             (conversation_id, ordinal, item_type, message_id) \
         VALUES (?1, ?2, 'message', ?3)",
        params![conversation_id, next_ord, message_id],
    )
    .map_err(|e| storage_err(&e))?;

    tx.execute(
        "UPDATE conversations SET updated_at = datetime('now') \
         WHERE conversation_id = ?1",
        [conversation_id],
    )
    .map_err(|e| storage_err(&e))?;

    tx.commit().map_err(|e| storage_err(&e))?;
    Ok(())
}

/// Enumerate every conversation with computed message + token totals.
fn list_sessions_sync(conn: &mut Connection) -> Result<Vec<SessionInfo>, EngineError> {
    let mut stmt = conn
        .prepare(
            "SELECT c.name, \
                    (SELECT COUNT(*) FROM context_items \
                     WHERE conversation_id = c.conversation_id), \
                    (SELECT COALESCE(SUM(m.token_count), 0) \
                          + COALESCE(SUM(s.token_count), 0) \
                     FROM context_items ci \
                     LEFT JOIN messages  m ON ci.message_id = m.message_id \
                     LEFT JOIN summaries s ON ci.summary_id = s.summary_id \
                     WHERE ci.conversation_id = c.conversation_id) \
             FROM conversations c \
             ORDER BY c.name",
        )
        .map_err(|e| storage_err(&e))?;

    let rows = stmt
        .query_map([], |row| {
            let stem: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            let tokens: i64 = row.get(2)?;
            Ok(SessionInfo {
                name: desanitize_name(&stem),
                message_count: usize::try_from(count).unwrap_or(0),
                estimated_tokens: usize::try_from(tokens).unwrap_or(0),
            })
        })
        .map_err(|e| storage_err(&e))?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| storage_err(&e))?);
    }
    Ok(out)
}

/// Walk `context_items` in order, rehydrate each `'message'` row back
/// into a `Message`. Prepends the system prompt. `'summary'` rows are
/// ignored — once compaction lands they will produce a synthetic
/// system message with the summary content + recall guidance.
fn assemble_sync(
    conn: &Connection,
    conversation_id: i64,
    system_prompt: &str,
) -> Result<AssembledContext, EngineError> {
    let mut stmt = conn
        .prepare(
            "SELECT m.message_id, m.role, m.content \
             FROM context_items ci \
             JOIN messages m ON ci.message_id = m.message_id \
             WHERE ci.conversation_id = ?1 AND ci.item_type = 'message' \
             ORDER BY ci.ordinal",
        )
        .map_err(|e| storage_err(&e))?;

    let rows = stmt
        .query_map([conversation_id], |r| {
            let id: i64 = r.get(0)?;
            let role: String = r.get(1)?;
            let content: String = r.get(2)?;
            Ok((id, role, content))
        })
        .map_err(|e| storage_err(&e))?;

    let mut entries: Vec<(i64, String, String)> = Vec::new();
    for r in rows {
        entries.push(r.map_err(|e| storage_err(&e))?);
    }

    let mut messages = Vec::with_capacity(entries.len() + 1);
    messages.push(Message::System {
        content: system_prompt.to_string(),
    });
    for (message_id, role, content) in entries {
        messages.push(reconstruct_message(conn, message_id, &role, content)?);
    }
    Ok(AssembledContext { messages })
}

/// Rebuild a `Message` from its row plus its `message_parts`.
///
/// `messages.content` already stores the canonical text payload (the
/// flattened `Message::content()` value), so for `user`/`system`
/// variants it's a direct wrap. `tool` looks up its `call_id` from the
/// single `tool_output` part. `assistant` is split: if the message has
/// any `tool_call` parts it becomes [`Message::ToolCalls`], otherwise
/// a plain [`Message::Assistant`].
fn reconstruct_message(
    conn: &Connection,
    message_id: i64,
    role: &str,
    content: String,
) -> Result<Message, EngineError> {
    match role {
        "user" => Ok(Message::User { content }),
        "system" => Ok(Message::System { content }),
        "tool" => {
            let call_id: String = conn
                .query_row(
                    "SELECT tool_call_id FROM message_parts \
                     WHERE message_id = ?1 AND part_type = 'tool_output'",
                    [message_id],
                    |r| r.get(0),
                )
                .map_err(|e| storage_err(&e))?;
            Ok(Message::Tool { call_id, content })
        }
        "assistant" => {
            let mut stmt = conn
                .prepare(
                    "SELECT tool_call_id, tool_name, tool_input \
                     FROM message_parts \
                     WHERE message_id = ?1 AND part_type = 'tool_call' \
                     ORDER BY ordinal",
                )
                .map_err(|e| storage_err(&e))?;

            let calls: Vec<ToolCall> = stmt
                .query_map([message_id], |r| {
                    let id: String = r.get(0)?;
                    let name: String = r.get(1)?;
                    let arguments: String = r.get(2)?;
                    Ok(ToolCall::new(id, ToolFunction { name, arguments }))
                })
                .map_err(|e| storage_err(&e))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| storage_err(&e))?;

            if calls.is_empty() {
                Ok(Message::Assistant { content })
            } else {
                Ok(Message::ToolCalls { content, calls })
            }
        }
        other => Err(EngineError::Storage(format!(
            "unknown message role: {other}"
        ))),
    }
}

fn role_str(msg: &Message) -> &'static str {
    match msg {
        Message::User { .. } => "user",
        Message::Assistant { .. } | Message::ToolCalls { .. } => "assistant",
        Message::Tool { .. } => "tool",
        Message::System { .. } => "system",
    }
}

/// Decompose a `Message` into rows in `message_parts`.
///
/// Each kitaebot variant maps to one or more rows per spec 14
/// "Message parts" table. `part_id` is `part_<message_id>_<ordinal>`,
/// deterministic so re-running an ingest path on a replayed session
/// would collide rather than silently double-write.
fn insert_parts(
    tx: &rusqlite::Transaction<'_>,
    message_id: i64,
    msg: &Message,
) -> Result<(), EngineError> {
    match msg {
        Message::User { content }
        | Message::Assistant { content }
        | Message::System { content } => {
            insert_text_part(tx, message_id, 0, content)?;
        }
        Message::Tool { call_id, content } => {
            tx.execute(
                "INSERT INTO message_parts \
                     (part_id, message_id, part_type, ordinal, \
                      text_content, tool_call_id) \
                 VALUES (?1, ?2, 'tool_output', 0, ?3, ?4)",
                params![part_id(message_id, 0), message_id, content, call_id],
            )
            .map_err(|e| storage_err(&e))?;
        }
        Message::ToolCalls { content, calls } => {
            insert_text_part(tx, message_id, 0, content)?;
            for (i, tc) in calls.iter().enumerate() {
                let ord = i64::try_from(i + 1).unwrap_or(i64::MAX);
                tx.execute(
                    "INSERT INTO message_parts \
                         (part_id, message_id, part_type, ordinal, \
                          tool_call_id, tool_name, tool_input) \
                     VALUES (?1, ?2, 'tool_call', ?3, ?4, ?5, ?6)",
                    params![
                        part_id(message_id, ord),
                        message_id,
                        ord,
                        tc.id,
                        tc.function.name,
                        tc.function.arguments,
                    ],
                )
                .map_err(|e| storage_err(&e))?;
            }
        }
    }
    Ok(())
}

fn insert_text_part(
    tx: &rusqlite::Transaction<'_>,
    message_id: i64,
    ordinal: i64,
    content: &str,
) -> Result<(), EngineError> {
    tx.execute(
        "INSERT INTO message_parts \
             (part_id, message_id, part_type, ordinal, text_content) \
         VALUES (?1, ?2, 'text', ?3, ?4)",
        params![part_id(message_id, ordinal), message_id, ordinal, content],
    )
    .map_err(|e| storage_err(&e))?;
    Ok(())
}

fn part_id(message_id: i64, ordinal: i64) -> String {
    format!("part_{message_id}_{ordinal}")
}

/// Look up (or create) a conversation by name. Returns its id.
fn ensure_conversation(conn: &Connection, name: &str) -> Result<i64, EngineError> {
    conn.execute(
        "INSERT OR IGNORE INTO conversations (name, created_at, updated_at) \
         VALUES (?1, datetime('now'), datetime('now'))",
        [name],
    )
    .map_err(|e| storage_err(&e))?;
    conn.query_row(
        "SELECT conversation_id FROM conversations WHERE name = ?1",
        [name],
        |row| row.get(0),
    )
    .map_err(|e| storage_err(&e))
}

fn storage_err(e: &rusqlite::Error) -> EngineError {
    EngineError::Storage(e.to_string())
}

// ── Name sanitization ───────────────────────────────────────────────
//
// Mirrors `engine::flat`. Kept as a duplicate for now — when a third
// engine needs the same logic, lift into `engine::names`.

fn sanitize_name(name: &str) -> String {
    name.replace('\0', "").replace("..", "").replace('/', "--")
}

fn desanitize_name(stem: &str) -> String {
    stem.replace("--", "/")
}

// ── Active session persistence ──────────────────────────────────────

fn read_active_session(memory_dir: &Path) -> Option<String> {
    let path = memory_dir.join("active_session");
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn persist_active_session(memory_dir: &Path, name: &str) {
    let path = memory_dir.join("active_session");
    let tmp = memory_dir.join("active_session.tmp");
    if fs::write(&tmp, name).is_ok() {
        let _ = fs::rename(&tmp, &path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ToolCall, ToolFunction};

    fn temp_engine() -> (LcmEngine, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("lcm.db");
        let memory_dir = dir.path().join("memory");
        fs::create_dir_all(&memory_dir).unwrap();
        let engine = LcmEngine::new(&db_path, memory_dir, ContextConfig::default()).unwrap();
        (engine, dir)
    }

    #[tokio::test]
    async fn push_message_persists_row_and_context_item() {
        let (mut engine, _dir) = temp_engine();
        engine
            .push_message(Message::User {
                content: "hello".into(),
            })
            .await
            .unwrap();

        let conn = engine.conn.lock().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);

        let ci_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM context_items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ci_count, 1);
    }

    #[tokio::test]
    async fn push_message_sequences_within_conversation() {
        let (mut engine, _dir) = temp_engine();
        for i in 0..3 {
            engine
                .push_message(Message::User {
                    content: format!("msg {i}"),
                })
                .await
                .unwrap();
        }

        let conn = engine.conn.lock().unwrap();
        let seqs: Vec<i64> = conn
            .prepare("SELECT seq FROM messages ORDER BY seq")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(seqs, vec![0, 1, 2]);
    }

    #[tokio::test]
    async fn push_tool_calls_decomposes_parts() {
        let (mut engine, _dir) = temp_engine();
        engine
            .push_message(Message::ToolCalls {
                content: "thinking".into(),
                calls: vec![
                    ToolCall::new(
                        "c1".into(),
                        ToolFunction {
                            name: "exec".into(),
                            arguments: r#"{"cmd":"ls"}"#.into(),
                        },
                    ),
                    ToolCall::new(
                        "c2".into(),
                        ToolFunction {
                            name: "read".into(),
                            arguments: r#"{"path":"a"}"#.into(),
                        },
                    ),
                ],
            })
            .await
            .unwrap();

        let conn = engine.conn.lock().unwrap();
        let parts: Vec<(String, String, Option<String>)> = conn
            .prepare(
                "SELECT part_type, COALESCE(text_content,''), tool_name \
                 FROM message_parts ORDER BY ordinal",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();

        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].0, "text");
        assert_eq!(parts[0].1, "thinking");
        assert_eq!(parts[1].0, "tool_call");
        assert_eq!(parts[1].2.as_deref(), Some("exec"));
        assert_eq!(parts[2].0, "tool_call");
        assert_eq!(parts[2].2.as_deref(), Some("read"));
    }

    #[tokio::test]
    async fn push_tool_result_records_call_id() {
        let (mut engine, _dir) = temp_engine();
        engine
            .push_message(Message::Tool {
                call_id: "c1".into(),
                content: "result".into(),
            })
            .await
            .unwrap();

        let (kind, text, call_id): (String, String, String) = engine
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT part_type, text_content, tool_call_id FROM message_parts",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(kind, "tool_output");
        assert_eq!(text, "result");
        assert_eq!(call_id, "c1");
    }

    #[tokio::test]
    async fn stats_reflects_context_items() {
        let (mut engine, _dir) = temp_engine();
        let initial = engine.stats();
        assert_eq!(initial.message_count, 0);
        assert_eq!(initial.token_estimate, 0);

        engine
            .push_message(Message::User {
                content: "a".repeat(40),
            })
            .await
            .unwrap();
        let after = engine.stats();
        assert_eq!(after.message_count, 1);
        assert_eq!(after.token_estimate, 10); // 40 chars / 4
    }

    #[tokio::test]
    async fn clear_drops_context_items_but_keeps_messages() {
        let (mut engine, _dir) = temp_engine();
        engine
            .push_message(Message::User {
                content: "kept".into(),
            })
            .await
            .unwrap();

        engine.clear().await.unwrap();

        // Active context is empty.
        assert_eq!(engine.stats().message_count, 0);

        // But the raw store still has the row.
        let messages: i64 = engine
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(messages, 1);
    }

    #[tokio::test]
    async fn switch_session_creates_and_isolates() {
        let (mut engine, _dir) = temp_engine();
        engine
            .push_message(Message::User {
                content: "in general".into(),
            })
            .await
            .unwrap();

        engine.switch_session("project-a").await.unwrap();
        assert_eq!(engine.active_session(), "project-a");
        assert_eq!(engine.stats().message_count, 0);

        engine
            .push_message(Message::User {
                content: "in project-a".into(),
            })
            .await
            .unwrap();

        engine.switch_session("general").await.unwrap();
        assert_eq!(engine.stats().message_count, 1);
    }

    #[tokio::test]
    async fn switch_session_idempotent() {
        let (mut engine, _dir) = temp_engine();
        engine
            .push_message(Message::User {
                content: "x".into(),
            })
            .await
            .unwrap();
        engine.switch_session("general").await.unwrap();
        assert_eq!(engine.stats().message_count, 1);
    }

    #[tokio::test]
    async fn switch_session_persists_active_name() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("lcm.db");
        let memory_dir = dir.path().join("memory");
        fs::create_dir_all(&memory_dir).unwrap();

        {
            let mut engine =
                LcmEngine::new(&db_path, memory_dir.clone(), ContextConfig::default()).unwrap();
            engine.switch_session("kitaebot").await.unwrap();
        }

        let engine = LcmEngine::new(&db_path, memory_dir, ContextConfig::default()).unwrap();
        assert_eq!(engine.active_session(), "kitaebot");
    }

    #[tokio::test]
    async fn list_sessions_enumerates_all_conversations() {
        let (mut engine, _dir) = temp_engine();
        engine
            .push_message(Message::User {
                content: "g".into(),
            })
            .await
            .unwrap();
        engine.switch_session("beta").await.unwrap();
        engine
            .push_message(Message::User {
                content: "b1".into(),
            })
            .await
            .unwrap();
        engine
            .push_message(Message::User {
                content: "b2".into(),
            })
            .await
            .unwrap();

        let sessions = engine.list_sessions().await.unwrap();
        let names: Vec<&str> = sessions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"general"));
        assert!(names.contains(&"beta"));

        let beta = sessions.iter().find(|s| s.name == "beta").unwrap();
        assert_eq!(beta.message_count, 2);
    }

    #[tokio::test]
    async fn save_is_no_op() {
        let (mut engine, _dir) = temp_engine();
        engine
            .push_message(Message::User {
                content: "x".into(),
            })
            .await
            .unwrap();
        engine.save().await.unwrap();
        assert_eq!(engine.stats().message_count, 1);
    }

    #[tokio::test]
    async fn slashed_session_name_sanitized_to_double_dash() {
        let (mut engine, _dir) = temp_engine();
        engine.switch_session("owner/repo").await.unwrap();
        assert_eq!(engine.active_session(), "owner--repo");

        let sessions = engine.list_sessions().await.unwrap();
        // The list view reverses sanitization for display.
        assert!(sessions.iter().any(|s| s.name == "owner/repo"));
    }

    #[tokio::test]
    async fn assemble_prepends_system_and_preserves_order() {
        let (mut engine, _dir) = temp_engine();
        engine
            .push_message(Message::User {
                content: "u1".into(),
            })
            .await
            .unwrap();
        engine
            .push_message(Message::Assistant {
                content: "a1".into(),
            })
            .await
            .unwrap();
        engine
            .push_message(Message::User {
                content: "u2".into(),
            })
            .await
            .unwrap();

        let ctx = engine.assemble("SYS").await.unwrap();
        assert_eq!(ctx.messages.len(), 4);
        match &ctx.messages[0] {
            Message::System { content } => assert_eq!(content, "SYS"),
            other => panic!("expected system, got {other:?}"),
        }
        match &ctx.messages[1] {
            Message::User { content } => assert_eq!(content, "u1"),
            other => panic!("expected user, got {other:?}"),
        }
        match &ctx.messages[2] {
            Message::Assistant { content } => assert_eq!(content, "a1"),
            other => panic!("expected assistant, got {other:?}"),
        }
        match &ctx.messages[3] {
            Message::User { content } => assert_eq!(content, "u2"),
            other => panic!("expected user, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn assemble_roundtrips_tool_call_messages() {
        let (mut engine, _dir) = temp_engine();
        let calls = vec![
            ToolCall::new(
                "c1".into(),
                ToolFunction {
                    name: "exec".into(),
                    arguments: r#"{"cmd":"ls"}"#.into(),
                },
            ),
            ToolCall::new(
                "c2".into(),
                ToolFunction {
                    name: "read".into(),
                    arguments: r#"{"path":"a"}"#.into(),
                },
            ),
        ];
        engine
            .push_message(Message::ToolCalls {
                content: "thinking".into(),
                calls: calls.clone(),
            })
            .await
            .unwrap();
        engine
            .push_message(Message::Tool {
                call_id: "c1".into(),
                content: "ls output".into(),
            })
            .await
            .unwrap();

        let ctx = engine.assemble("SYS").await.unwrap();
        match &ctx.messages[1] {
            Message::ToolCalls {
                content,
                calls: round,
            } => {
                assert_eq!(content, "thinking");
                assert_eq!(round.len(), 2);
                assert_eq!(round[0].id, "c1");
                assert_eq!(round[0].function.name, "exec");
                assert_eq!(round[0].function.arguments, r#"{"cmd":"ls"}"#);
                assert_eq!(round[1].id, "c2");
                assert_eq!(round[1].function.name, "read");
            }
            other => panic!("expected tool calls, got {other:?}"),
        }
        match &ctx.messages[2] {
            Message::Tool { call_id, content } => {
                assert_eq!(call_id, "c1");
                assert_eq!(content, "ls output");
            }
            other => panic!("expected tool, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn assemble_after_clear_only_has_system() {
        let (mut engine, _dir) = temp_engine();
        engine
            .push_message(Message::User {
                content: "kept".into(),
            })
            .await
            .unwrap();
        engine.clear().await.unwrap();
        let ctx = engine.assemble("SYS").await.unwrap();
        assert_eq!(ctx.messages.len(), 1);
        assert!(matches!(&ctx.messages[0], Message::System { .. }));
    }

    #[tokio::test]
    async fn assemble_isolates_per_session() {
        let (mut engine, _dir) = temp_engine();
        engine
            .push_message(Message::User {
                content: "in general".into(),
            })
            .await
            .unwrap();
        engine.switch_session("other").await.unwrap();
        engine
            .push_message(Message::User {
                content: "in other".into(),
            })
            .await
            .unwrap();

        let ctx = engine.assemble("SYS").await.unwrap();
        assert_eq!(ctx.messages.len(), 2);
        match &ctx.messages[1] {
            Message::User { content } => assert_eq!(content, "in other"),
            other => panic!("expected user, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn compact_methods_return_not_implemented() {
        let (mut engine, _dir) = temp_engine();
        let summarize: SummarizeFn = Box::new(|_| Box::pin(async { Ok(String::new()) }));
        assert!(engine.compact_if_needed(&summarize).await.is_err());
        assert!(engine.force_compact(&summarize).await.is_err());
    }
}
