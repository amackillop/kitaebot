//! [`LcmEngine`] — `SQLite`-backed LCM context engine.
//!
//! Every push persists a row in `messages` (decomposed into
//! `message_parts`) and appends a `message`-kind item to
//! `context_items`. `assemble` walks `context_items` in order and
//! rehydrates each row back into a `Message` from `messages` + parts.
//! Compaction (`compaction.rs`) folds old context items into
//! `summaries` rows; `assemble` renders those as synthetic system
//! messages with recall guidance.
//!
//! Oversized user and tool messages are intercepted at ingest: the
//! raw payload goes to `state/lcm/payloads/<file_id>` on disk, a
//! `large_files` row records its metadata, and `messages.content`
//! stores a compact `<file>` reference with an exploration summary
//! (see `explore.rs`). The reference plus the on-disk payload
//! together remain the source of truth. User messages threshold on
//! `lcm.large_file_threshold` with an LLM summary; tool results on
//! the lower `context.tool_output_tokens` with a free head+tail
//! excerpt.
//!
//! Active session persistence reuses `state/active_session` — the
//! same plain-text file flat sessions write to, so switching engines
//! preserves the user's last session.
//!
//! Names are sanitized identically to flat sessions (`/` -> `--`)
//! because GitHub channel routing produces `owner/repo` strings; the
//! sanitization keeps them as legal `conversations.name` values.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};
use tracing::{error, info};

use crate::config::ContextConfig;
use crate::error::{EngineError, InvalidToolName};
use crate::tools::Tool;
use crate::types::{Message, ToolCall, ToolFunction, estimate_tokens};

use super::super::names::{desanitize_name, sanitize_name};
use super::super::{
    AssembledContext, CompactionEvent, ContextEngine, ContextStats, SessionInfo, SummarizeFn,
    ToolScope,
};
use super::compaction;
use super::explore;
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
///
/// Background compaction (the soft-threshold path) opens its own
/// `Connection` rather than sharing this mutex, so concurrent reads
/// from the actor go through unimpeded; WAL handles isolation.
pub struct LcmEngine {
    conn: Arc<Mutex<Connection>>,
    db_path: PathBuf,
    active_name: String,
    conversation_id: i64,
    /// Shared with retrieval tools so they can target the current
    /// session without holding a reference to the engine. Updated
    /// atomically on every successful [`switch_session`] call.
    active_id: Arc<AtomicI64>,
    context_dir: PathBuf,
    ctx: ContextConfig,
    /// Async compaction in flight (soft-threshold path). Set when a
    /// turn crosses the soft threshold without crossing the hard
    /// threshold; drained at the start of the next compaction call.
    /// Summarizer for exploration summaries of externalized
    /// plain-text payloads. Injected at construction; compaction
    /// receives its own via method arguments.
    summarize: SummarizeFn,
    /// Provider-reported prompt size of the last request, if any.
    /// Cleared whenever the context shrinks (compaction, clear,
    /// session switch) — a stale high-water mark would re-trigger
    /// compaction forever via `max()`.
    observed_tokens: Option<usize>,
}

impl LcmEngine {
    /// Open or create the LCM database at `db_path`.
    ///
    /// Restores the active session from `state/active_session` (or
    /// falls back to `"general"`), ensuring a `conversations` row
    /// exists for it.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Storage`] if the database cannot be
    /// opened or the active conversation row cannot be created.
    pub fn new(
        context_dir: &Path,
        ctx: ContextConfig,
        summarize: SummarizeFn,
    ) -> Result<Self, EngineError> {
        // Each engine namespaces its own subdirectory: switching
        // backends can never clobber another engine's files (spec 14).
        let context_dir = context_dir.join(crate::workspace::LCM_DIR);
        std::fs::create_dir_all(&context_dir).map_err(|e| EngineError::Io {
            operation: "create",
            path: context_dir.clone(),
            source: e,
        })?;
        let db_path = context_dir.join("lcm.db");
        let conn = schema::open(&db_path)?;
        let active_name = read_active_session(&context_dir).unwrap_or_else(|| "general".into());
        let conversation_id = ensure_conversation(&conn, &active_name)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            db_path,
            active_name,
            conversation_id,
            active_id: Arc::new(AtomicI64::new(conversation_id)),
            context_dir,
            ctx,
            summarize,
            observed_tokens: None,
        })
    }

    /// Soft compaction trigger: percent of `max_tokens` at which the
    /// engine starts a background compaction. Reported as `budget`
    /// in [`ContextStats`] because that field's semantic is "at this
    /// point we begin compacting".
    fn soft_threshold(&self) -> usize {
        self.ctx.max_tokens as usize * self.ctx.lcm.soft_budget_percent as usize / 100
    }

    /// Hard compaction trigger: percent of `max_tokens` at which the
    /// engine must compact synchronously before the next provider call.
    fn hard_threshold(&self) -> usize {
        self.ctx.max_tokens as usize * self.ctx.lcm.hard_budget_percent as usize / 100
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

    /// Intercept oversized user and tool payloads before storage.
    ///
    /// Oversized content is externalized: the message comes back with
    /// its content replaced by a `<file>` reference, alongside the
    /// `large_files` row to insert with it. User messages threshold on
    /// `lcm.large_file_threshold` and get the LLM exploration summary;
    /// tool results threshold on the much lower `tool_output_tokens`
    /// and get a free mechanical excerpt, because they arrive on every
    /// turn. Everything else passes through untouched.
    async fn intercept_large(
        &mut self,
        msg: Message,
    ) -> Result<(Message, Option<LargeFileRow>), EngineError> {
        let user_threshold = self.ctx.lcm.large_file_threshold as usize;
        let tool_threshold = self.ctx.tool_output_tokens as usize;
        match msg {
            Message::User { content } if estimate_tokens(&content) > user_threshold => {
                let (reference, row) = self
                    .externalize(&content, None, SummaryStrategy::Explore)
                    .await?;
                Ok((Message::User { content: reference }, Some(row)))
            }
            Message::Tool { call_id, content } if estimate_tokens(&content) > tool_threshold => {
                let hint = self.file_read_path_hint(&call_id).await;
                let (reference, row) = self
                    .externalize(&content, hint, SummaryStrategy::Mechanical)
                    .await?;
                Ok((
                    Message::Tool {
                        call_id,
                        content: reference,
                    },
                    Some(row),
                ))
            }
            other => Ok((other, None)),
        }
    }

    /// Path argument of the `file_read` call this tool result answers,
    /// if that is what produced it. The originating assistant message
    /// is already persisted as a `tool_call` part linked by `call_id`,
    /// so the hint comes from the store rather than from state carried
    /// between pushes. Only consulted for over-threshold tool results.
    async fn file_read_path_hint(&self, call_id: &str) -> Option<String> {
        let conn = Arc::clone(&self.conn);
        let conversation_id = self.conversation_id;
        let call_id = call_id.to_string();
        run_blocking(conn, move |c| {
            Ok(lookup_file_read_path(c, conversation_id, &call_id))
        })
        .await
        .ok()
        .flatten()
    }

    /// Directory holding externalized payloads.
    fn payloads_dir(&self) -> PathBuf {
        self.context_dir.join(crate::workspace::LCM_PAYLOADS_DIR)
    }

    /// Write `content` to `context/lcm/payloads/<file_id>`, generate
    /// its exploration summary, and return the `<file>` reference
    /// plus the metadata row for `large_files`.
    async fn externalize(
        &self,
        content: &str,
        path_hint: Option<String>,
        strategy: SummaryStrategy,
    ) -> Result<(String, LargeFileRow), EngineError> {
        let file_id = explore::file_id(content);
        let payload_dir = self.payloads_dir();
        tokio::fs::create_dir_all(&payload_dir)
            .await
            .map_err(|e| EngineError::Io {
                operation: "create",
                path: payload_dir.clone(),
                source: e,
            })?;
        let payload_path = payload_dir.join(&file_id);
        tokio::fs::write(&payload_path, content)
            .await
            .map_err(|e| EngineError::Io {
                operation: "write",
                path: payload_path.clone(),
                source: e,
            })?;

        // The payload on disk stays a verbatim copy of the tool
        // result, but detection and exploration see the underlying
        // file content: `file_read` line numbering would otherwise
        // break every structured parser.
        let unframed = explore::strip_tool_framing(content);
        let kind = explore::detect_kind(path_hint.as_deref(), &unframed);
        let summary = match strategy {
            SummaryStrategy::Explore => {
                explore::exploration_summary(
                    &unframed,
                    path_hint.as_deref(),
                    &self.summarize,
                    self.ctx.lcm.large_file_summary_tokens,
                )
                .await
            }
            SummaryStrategy::Mechanical => explore::mechanical_excerpt(&unframed),
        };

        let token_count = estimate_tokens(content);
        info!(
            file_id,
            tokens = token_count,
            kind = ?kind,
            path = path_hint.as_deref().unwrap_or("(none)"),
            "externalizing oversized payload"
        );
        // With no original path, point at the stored payload —
        // workspace-relative, so the confined file tools accept it.
        let path = path_hint.unwrap_or_else(|| {
            use crate::workspace::{CONTEXT_DIR, LCM_DIR, LCM_PAYLOADS_DIR};
            format!("{CONTEXT_DIR}/{LCM_DIR}/{LCM_PAYLOADS_DIR}/{file_id}")
        });
        let reference = explore::format_file_reference(&file_id, &path, token_count, &summary);
        let row = LargeFileRow {
            file_id,
            path,
            mime_type: explore::mime_hint(kind).to_string(),
            byte_size: i64::try_from(content.len()).unwrap_or(i64::MAX),
            token_count: i64::try_from(token_count).unwrap_or(i64::MAX),
            exploration_summary: summary,
        };
        Ok((reference, row))
    }
}

/// How the exploration summary of an externalized payload is built.
enum SummaryStrategy {
    /// Type-aware exploration; plain text may call the LLM.
    /// Used for user payloads, which are rare and worth the spend.
    Explore,
    /// Head+tail excerpt, no LLM call. Used for tool output, which
    /// is too frequent to summarize per event.
    Mechanical,
}

/// Metadata for a `large_files` insert, produced by
/// [`LcmEngine::externalize`] and written in the same transaction as
/// its message.
struct LargeFileRow {
    file_id: String,
    path: String,
    mime_type: String,
    byte_size: i64,
    token_count: i64,
    exploration_summary: String,
}

impl ContextEngine for LcmEngine {
    async fn push_message(&mut self, msg: Message) -> Result<(), EngineError> {
        let (msg, large_file) = self.intercept_large(msg).await?;
        let conversation_id = self.conversation_id;
        let conn = Arc::clone(&self.conn);
        run_blocking(conn, move |c| {
            push_message_sync(c, conversation_id, &msg, large_file.as_ref())
        })
        .await
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

    fn observe_tokens(&mut self, prompt_tokens: usize) {
        self.observed_tokens = Some(prompt_tokens);
    }

    async fn compact_if_urgent(
        &mut self,
        summarize: &SummarizeFn,
    ) -> Result<Option<CompactionEvent>, EngineError> {
        // The hard threshold only: this runs before every completion,
        // and compacting here cold-starts the prompt cache for the
        // rest of the turn. Routine shrinking waits for the turn
        // boundary (`compact_between_turns`).
        let tokens = self.stats().token_estimate;
        if tokens < self.hard_threshold() {
            return Ok(None);
        }
        info!(
            tokens,
            "hard threshold reached; running blocking compaction"
        );
        let event = compaction::run_compaction(
            Arc::clone(&self.conn),
            self.conversation_id,
            self.ctx.lcm,
            summarize,
        )
        .await?;
        self.observed_tokens = None;
        Ok(Some(event))
    }

    async fn compact_between_turns(
        &mut self,
        summarize: &SummarizeFn,
    ) -> Result<Option<CompactionEvent>, EngineError> {
        let tokens = self.stats().token_estimate;
        if tokens < self.soft_threshold() {
            return Ok(None);
        }
        info!(tokens, "soft threshold reached; compacting between turns");
        let event = compaction::run_compaction(
            Arc::clone(&self.conn),
            self.conversation_id,
            self.ctx.lcm,
            summarize,
        )
        .await?;
        self.observed_tokens = None;
        Ok(Some(event))
    }

    async fn force_compact(
        &mut self,
        summarize: &SummarizeFn,
    ) -> Result<CompactionEvent, EngineError> {
        let event = compaction::run_compaction(
            Arc::clone(&self.conn),
            self.conversation_id,
            self.ctx.lcm,
            summarize,
        )
        .await?;
        self.observed_tokens = None;
        Ok(event)
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
            )?;
            Ok(())
        })
        .await?;
        self.observed_tokens = None;
        Ok(())
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
            // Best available count: the stored estimate or the
            // provider-observed prompt size, whichever is larger.
            // Both undercount (estimates are char/4 and miss the
            // system prompt; the observation lags one turn).
            // `compact_if_urgent` reads this, so the observation
            // feeds the thresholds too.
            token_estimate: usize::try_from(tokens)
                .unwrap_or(0)
                .max(self.observed_tokens.unwrap_or(0)),
            // Reported budget is the soft threshold: the level at which
            // compaction first kicks in. The hard threshold above it
            // exists to bound the worst case but isn't user-facing.
            budget: self.soft_threshold(),
        }
    }

    fn tools(&self, scope: ToolScope) -> Vec<Arc<dyn Tool>> {
        // Open independent read-only connections — one per tool.
        // WAL lets these readers run concurrently with the engine's
        // writer. If a connection fails to open, log and skip that
        // tool: a missing retrieval tool degrades gracefully (the
        // model still has the active context), whereas panicking here
        // would take down the daemon for a non-essential feature.
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        let open = |label: &'static str| -> Option<Connection> {
            schema::open_readonly(&self.db_path)
                .map_err(|e| error!(tool = label, "failed to open LCM tool connection: {e}"))
                .ok()
        };
        if let Some(conn) = open("lcm_grep") {
            tools.push(Arc::new(LcmGrep::new(conn, Arc::clone(&self.active_id))));
        }
        if let Some(conn) = open("lcm_describe") {
            tools.push(Arc::new(LcmDescribe::new(
                conn,
                Arc::clone(&self.active_id),
            )));
        }
        // Bulk expansion is sub-agent-only: expanding a summary into
        // the root context would flood the very window LCM manages.
        // The parent delegates via the task tool instead (spec 19).
        if scope == ToolScope::SubAgent
            && let Some(conn) = open("lcm_expand")
        {
            tools.push(Arc::new(LcmExpand::new(
                conn,
                Arc::clone(&self.active_id),
                self.payloads_dir(),
            )));
        }
        tools
    }

    async fn report(&self) -> Result<String, EngineError> {
        let conn = Arc::clone(&self.conn);
        run_blocking(conn, |c| super::report::report_sync(c)).await
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
        self.observed_tokens = None;
        self.active_name = sanitized;
        self.conversation_id = id;
        self.active_id.store(id, Ordering::Release);
        persist_active_session(&self.context_dir, &self.active_name);
        Ok(())
    }

    async fn list_sessions(&self) -> Result<Vec<SessionInfo>, EngineError> {
        let conn = Arc::clone(&self.conn);
        run_blocking(conn, list_sessions_sync).await
    }

    async fn pending_distill_tokens(
        &self,
        since: &BTreeMap<String, u64>,
    ) -> Result<BTreeMap<String, u64>, EngineError> {
        let conn = Arc::clone(&self.conn);
        let since = since.clone();
        run_blocking(conn, move |c| pending_distill_tokens_sync(c, &since)).await
    }

    fn backup(context_dir: &Path, dest: &Path) -> Result<(), EngineError> {
        // lcm.db via VACUUM INTO, payload blobs and the cursor as
        // plain files; the shared snapshot handles both.
        crate::backup::snapshot_dir(context_dir, dest).map_err(|e| EngineError::Io {
            operation: "snapshot",
            path: context_dir.to_path_buf(),
            source: e,
        })
    }

    async fn latest_positions(&self) -> Result<BTreeMap<String, u64>, EngineError> {
        let conn = Arc::clone(&self.conn);
        run_blocking(conn, move |c| latest_positions_sync(c)).await
    }

    async fn transcript_since(
        &self,
        session: &str,
        after: u64,
        max_tokens: u64,
    ) -> Result<Vec<Message>, EngineError> {
        let conn = Arc::clone(&self.conn);
        let stem = sanitize_name(session);
        run_blocking(conn, move |c| {
            transcript_since_sync(c, &stem, after, max_tokens)
        })
        .await
    }
}

/// Run a blocking DB closure on Tokio's blocking pool.
///
/// Every async [`ContextEngine`] method that touches `SQLite` funnels
/// through here. The closure receives `&mut Connection` (locked from
/// the shared mutex) and returns a `Result<T, EngineError>`. A
/// `JoinError` from `spawn_blocking` is reported as `Storage`.
pub(super) async fn run_blocking<F, T>(conn: Arc<Mutex<Connection>>, f: F) -> Result<T, EngineError>
where
    F: FnOnce(&mut Connection) -> Result<T, EngineError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let mut guard = conn.lock().expect("LCM connection mutex poisoned");
        f(&mut guard)
    })
    .await
    .map_err(EngineError::Join)?
}

// ── Internal helpers ────────────────────────────────────────────────

/// Persist `msg` into `messages` + `message_parts` and append a
/// `'message'` row to `context_items`. When the message carries an
/// externalized payload, its `large_files` row lands in the same
/// transaction so a reference can never exist without its metadata.
fn push_message_sync(
    conn: &mut Connection,
    conversation_id: i64,
    msg: &Message,
    large_file: Option<&LargeFileRow>,
) -> Result<(), EngineError> {
    let role = role_str(msg);
    let content = msg.content().to_string();
    let token_count = i64::try_from(msg.token_estimate()).unwrap_or(i64::MAX);

    let tx = conn.transaction()?;

    let seq: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq), -1) + 1 FROM messages \
             WHERE conversation_id = ?1",
        [conversation_id],
        |row| row.get(0),
    )?;

    tx.execute(
        "INSERT INTO messages \
             (conversation_id, seq, role, content, token_count, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
        params![conversation_id, seq, role, content, token_count],
    )?;
    let message_id = tx.last_insert_rowid();

    insert_parts(&tx, message_id, msg)?;

    // Content-addressed file ids repeat when the same payload is seen
    // twice; keep the first row (and its first_seen_message_id).
    if let Some(f) = large_file {
        tx.execute(
            "INSERT OR IGNORE INTO large_files \
                 (file_id, conversation_id, path, mime_type, byte_size, \
                  token_count, exploration_summary, first_seen_message_id, \
                  created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, datetime('now'))",
            params![
                f.file_id,
                conversation_id,
                f.path,
                f.mime_type,
                f.byte_size,
                f.token_count,
                f.exploration_summary,
                message_id,
            ],
        )?;
    }

    let next_ord: i64 = tx.query_row(
        "SELECT COALESCE(MAX(ordinal), -1) + 1 FROM context_items \
             WHERE conversation_id = ?1",
        [conversation_id],
        |row| row.get(0),
    )?;
    tx.execute(
        "INSERT INTO context_items \
             (conversation_id, ordinal, item_type, message_id) \
         VALUES (?1, ?2, 'message', ?3)",
        params![conversation_id, next_ord, message_id],
    )?;

    tx.execute(
        "UPDATE conversations SET updated_at = datetime('now') \
         WHERE conversation_id = ?1",
        [conversation_id],
    )?;

    tx.commit()?;
    Ok(())
}

/// Enumerate every conversation with computed message + token totals.
fn list_sessions_sync(conn: &mut Connection) -> Result<Vec<SessionInfo>, EngineError> {
    let mut stmt = conn.prepare(
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
    )?;

    let rows = stmt.query_map([], |row| {
        let stem: String = row.get(0)?;
        let count: i64 = row.get(1)?;
        let tokens: i64 = row.get(2)?;
        Ok(SessionInfo {
            name: desanitize_name(&stem),
            message_count: usize::try_from(count).unwrap_or(0),
            estimated_tokens: usize::try_from(tokens).unwrap_or(0),
        })
    })?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Each conversation's next-unseen seq (`MAX(seq) + 1`), for priming
/// fresh distillation state at the current tips. Conversations with
/// no messages are omitted by the join.
fn latest_positions_sync(conn: &Connection) -> Result<BTreeMap<String, u64>, EngineError> {
    let mut stmt = conn.prepare(
        "SELECT c.name, MAX(m.seq) + 1 FROM conversations c \
             JOIN messages m ON m.conversation_id = c.conversation_id \
             GROUP BY c.conversation_id",
    )?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .map(|(stem, tip)| (desanitize_name(&stem), tip.cast_unsigned()))
        .collect())
}

/// Sum each conversation's undistilled `token_count` for the distill
/// gate. seq is monotonic and raw rows survive compaction, so the
/// count is a faithful high-water regardless of DAG state.
fn pending_distill_tokens_sync(
    conn: &Connection,
    since: &BTreeMap<String, u64>,
) -> Result<BTreeMap<String, u64>, EngineError> {
    let convs: Vec<(i64, String)> = {
        let mut stmt = conn.prepare("SELECT conversation_id, name FROM conversations")?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut out = BTreeMap::new();
    for (id, stem) in convs {
        let name = desanitize_name(&stem);
        let watermark = i64::try_from(since.get(&name).copied().unwrap_or(0)).unwrap_or(i64::MAX);
        let sum: i64 = conn.query_row(
            "SELECT COALESCE(SUM(token_count), 0) FROM messages \
                 WHERE conversation_id = ?1 AND seq >= ?2",
            params![id, watermark],
            |r| r.get(0),
        )?;
        if sum > 0 {
            out.insert(name, u64::try_from(sum).unwrap_or(u64::MAX));
        }
    }
    Ok(out)
}

/// Reconstruct one conversation's undistilled span, oldest first,
/// clamped to `max_tokens` (at least one event when any are pending).
/// A missing conversation yields an empty span.
fn transcript_since_sync(
    conn: &Connection,
    stem: &str,
    after: u64,
    max_tokens: u64,
) -> Result<Vec<Message>, EngineError> {
    let conversation_id: Option<i64> = conn
        .query_row(
            "SELECT conversation_id FROM conversations WHERE name = ?1",
            [stem],
            |r| r.get(0),
        )
        .optional()?;
    let Some(conversation_id) = conversation_id else {
        return Ok(Vec::new());
    };
    let after = i64::try_from(after).unwrap_or(i64::MAX);

    // Pass 1: cheap (seq, token_count) scan to find the cutoff. The
    // stored count is the same chars/4 estimate the clamp uses, so
    // the cutoff is exact — only the kept rows are materialized.
    let mut stmt = conn.prepare(
        "SELECT seq, token_count FROM messages \
             WHERE conversation_id = ?1 AND seq >= ?2 ORDER BY seq",
    )?;
    let mut rows = stmt.query(params![conversation_id, after])?;
    let mut cutoff: Option<i64> = None;
    let mut total: u64 = 0;
    let mut kept = false;
    while let Some(row) = rows.next()? {
        let tokens = u64::try_from(row.get::<_, i64>(1)?).unwrap_or(0);
        if kept && total + tokens > max_tokens {
            cutoff = Some(row.get::<_, i64>(0)?);
            break;
        }
        kept = true;
        total += tokens;
    }

    // Pass 2: full rows below the cutoff.
    let mut out = Vec::new();
    if kept {
        let mut stmt = conn.prepare(
            "SELECT message_id, role, content FROM messages \
                 WHERE conversation_id = ?1 AND seq >= ?2 AND seq < ?3 ORDER BY seq",
        )?;
        let cutoff = cutoff.unwrap_or(i64::MAX);
        let rows = stmt
            .query_map(params![conversation_id, after, cutoff], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, role, content) in rows {
            out.push(reconstruct_message(conn, id, &role, content)?);
        }
    }
    Ok(out)
}

/// Walk `context_items` in order, rebuild messages, and inject one
/// synthetic [`Message::System`] per summary item. The system prompt
/// is prepended, augmented with recall guidance whenever any summary
/// is present so the model knows it can drill back into the DAG via
/// the LCM tools.
enum AssembleRow {
    Message {
        id: i64,
        role: String,
        content: String,
    },
    Summary {
        id: String,
        kind: String,
        depth: i64,
        content: String,
        earliest_at: String,
        latest_at: String,
    },
}

/// Assemble the ordered message array for a provider call.
///
/// Reads the conversation's `context_items` in `ordinal` order and
/// rebuilds the provider-bound message list:
///
/// 1. A [`Message::System`] holding `system_prompt`, augmented with
///    [`RECALL_GUIDANCE`] when any summary item is present, so the model
///    knows it can drill back into the DAG via the LCM tools.
/// 2. Each context item in `ordinal` order, preserving the order it was
///    stored: message items are rebuilt via [`reconstruct_message`],
///    summary items become a synthetic [`Message::System`] wrapping the
///    summary in a `<summary>` tag.
///
/// Returns an [`AssembledContext`] whose `messages` are safe to send to
/// the provider in array order.
fn assemble_sync(
    conn: &Connection,
    conversation_id: i64,
    system_prompt: &str,
) -> Result<AssembledContext, EngineError> {
    let mut stmt = conn.prepare(
        "SELECT ci.item_type, \
                    m.message_id, m.role, m.content, \
                    s.summary_id, s.kind, s.depth, s.content, \
                    s.earliest_at, s.latest_at \
             FROM context_items ci \
             LEFT JOIN messages  m ON ci.message_id = m.message_id \
             LEFT JOIN summaries s ON ci.summary_id = s.summary_id \
             WHERE ci.conversation_id = ?1 \
             ORDER BY ci.ordinal",
    )?;

    let entries: Vec<AssembleRow> = stmt
        .query_map([conversation_id], |r| {
            let item_type: String = r.get(0)?;
            if item_type == "message" {
                Ok(AssembleRow::Message {
                    id: r.get(1)?,
                    role: r.get(2)?,
                    content: r.get(3)?,
                })
            } else {
                Ok(AssembleRow::Summary {
                    id: r.get(4)?,
                    kind: r.get(5)?,
                    depth: r.get(6)?,
                    content: r.get(7)?,
                    earliest_at: r.get(8)?,
                    latest_at: r.get(9)?,
                })
            }
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let has_summary = entries
        .iter()
        .any(|r| matches!(r, AssembleRow::Summary { .. }));

    let mut messages = Vec::with_capacity(entries.len() + 1);
    let mut system_content = system_prompt.to_string();
    if has_summary {
        system_content.push_str("\n\n");
        system_content.push_str(RECALL_GUIDANCE);
        if let Some(segment) = written_files_segment(conn, conversation_id)? {
            system_content.push_str("\n\n");
            system_content.push_str(&segment);
        }
    }
    messages.push(Message::System {
        content: system_content,
    });

    for row in entries {
        match row {
            AssembleRow::Message { id, role, content } => {
                messages.push(reconstruct_message(conn, id, &role, content)?);
            }
            AssembleRow::Summary {
                id,
                kind,
                depth,
                content,
                earliest_at,
                latest_at,
            } => {
                messages.push(Message::System {
                    content: format!(
                        "<summary id=\"{id}\" kind=\"{kind}\" depth=\"{depth}\" \
                         earliest_at=\"{earliest_at}\" latest_at=\"{latest_at}\">\n\
                         {content}\n\
                         </summary>"
                    ),
                });
            }
        }
    }
    Ok(AssembledContext { messages })
}

/// Cap on entries in the written-files recall segment.
const WRITTEN_FILES_CAP: usize = 30;

const WRITTEN_FILES_HEADER: &str = "## Files Written For This Request";

/// Distinct paths passed to `file_write`/`file_edit` since the newest
/// user message, newest first, capped at [`WRITTEN_FILES_CAP`].
///
/// Scoped to the current request, not the session: sessions are
/// long-lived, and a session-wide list goes stale across tickets and
/// workspace cleans. Returns `None` while every message after the pin
/// is still raw in context — the calls are visible and the segment
/// would be noise — and `None` when nothing was written.
fn written_files_segment(
    conn: &Connection,
    conversation_id: i64,
) -> Result<Option<String>, EngineError> {
    let pinned_seq: Option<i64> = conn.query_row(
        "SELECT MAX(seq) FROM messages \
             WHERE conversation_id = ?1 AND role = 'user'",
        [conversation_id],
        |r| r.get(0),
    )?;
    let Some(pinned_seq) = pinned_seq else {
        return Ok(None);
    };

    let compacted_after_pin: bool = conn.query_row(
        "SELECT EXISTS( \
                SELECT 1 FROM messages m \
                WHERE m.conversation_id = ?1 AND m.seq > ?2 \
                  AND NOT EXISTS( \
                    SELECT 1 FROM context_items ci \
                    WHERE ci.conversation_id = ?1 \
                      AND ci.item_type = 'message' \
                      AND ci.message_id = m.message_id))",
        params![conversation_id, pinned_seq],
        |r| r.get(0),
    )?;
    if !compacted_after_pin {
        return Ok(None);
    }

    let mut stmt = conn.prepare(
        "SELECT mp.tool_input FROM message_parts mp \
             JOIN messages m ON mp.message_id = m.message_id \
             WHERE m.conversation_id = ?1 AND m.seq > ?2 \
               AND mp.part_type = 'tool_call' \
               AND mp.tool_name IN ('file_write', 'file_edit') \
             ORDER BY m.seq DESC, mp.ordinal DESC",
    )?;
    let inputs: Vec<String> = stmt
        .query_map(params![conversation_id, pinned_seq], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut paths: Vec<String> = Vec::new();
    for input in inputs {
        let Some(path) = serde_json::from_str::<serde_json::Value>(&input)
            .ok()
            .as_ref()
            .and_then(|v| v.get("path"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        if !paths.contains(&path) {
            paths.push(path);
            if paths.len() == WRITTEN_FILES_CAP {
                break;
            }
        }
    }
    if paths.is_empty() {
        return Ok(None);
    }
    Ok(Some(format!(
        "{WRITTEN_FILES_HEADER}\n{}",
        paths.join("\n")
    )))
}

/// Recall guidance appended to the system prompt whenever the assembled
/// context contains any summary item. Mirrors spec 14 §"Context
/// Assembly".
const RECALL_GUIDANCE: &str = "\
## Compacted History

Summaries above are compressed context: maps to details, not the \
details themselves. Use retrieval tools before asserting specifics \
from summaries.

Tool escalation:
1. lcm_grep: search by keyword or regex
2. lcm_describe: inspect a specific summary's metadata and lineage
3. lcm_expand: drill into a summary to retrieve children or source \
messages (sub-agent only)

Do not guess exact values (commands, paths, SHAs, config) from \
condensed summaries. Use lcm_grep to search, or delegate expansion \
to a sub-agent.";

/// Rebuild a `Message` from its row plus its `message_parts`.
///
/// `messages.content` already stores the canonical text payload (the
/// flattened `Message::content()` value), so for `user`/`system`
/// variants it's a direct wrap. `tool` looks up its `call_id` from the
/// single `tool_output` part. `assistant` is split: if the message has
/// any `tool_call` parts it becomes [`Message::ToolCalls`], otherwise
/// a plain [`Message::Assistant`].
pub(super) fn reconstruct_message(
    conn: &Connection,
    message_id: i64,
    role: &str,
    content: String,
) -> Result<Message, EngineError> {
    match role {
        "user" => Ok(Message::User { content }),
        "system" => Ok(Message::System { content }),
        "tool" => {
            let call_id: String = conn.query_row(
                "SELECT tool_call_id FROM message_parts \
                     WHERE message_id = ?1 AND part_type = 'tool_output'",
                [message_id],
                |r| r.get(0),
            )?;
            Ok(Message::Tool { call_id, content })
        }
        "assistant" => {
            let mut stmt = conn.prepare(
                "SELECT tool_call_id, tool_name, tool_input \
                     FROM message_parts \
                     WHERE message_id = ?1 AND part_type = 'tool_call' \
                     ORDER BY ordinal",
            )?;

            let rows: Vec<(String, String, String)> = stmt
                .query_map([message_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            // Names are validated before they are ever written, so a
            // stored one that no longer parses means the row came from
            // somewhere other than ingest: the row is corrupt, and
            // saying so beats replaying it into a request that 400s.
            let calls = rows
                .into_iter()
                .map(|(id, name, arguments)| {
                    let name = name.parse().map_err(|e: InvalidToolName| {
                        EngineError::Storage(format!("message {message_id}: {e}"))
                    })?;
                    Ok(ToolCall::new(id, ToolFunction { name, arguments }))
                })
                .collect::<Result<Vec<_>, EngineError>>()?;

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
            )?;
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
                        tc.function.name.as_str(),
                        tc.function.arguments,
                    ],
                )?;
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
    )?;
    Ok(())
}

fn part_id(message_id: i64, ordinal: i64) -> String {
    format!("part_{message_id}_{ordinal}")
}

/// Find the `path` argument of the `file_read` tool call with the
/// given `call_id` in this conversation. `None` when the result came
/// from a different tool, the call is not stored, or the arguments
/// do not parse.
fn lookup_file_read_path(conn: &Connection, conversation_id: i64, call_id: &str) -> Option<String> {
    let tool_input: String = conn
        .query_row(
            "SELECT p.tool_input FROM message_parts p \
             JOIN messages m ON p.message_id = m.message_id \
             WHERE p.tool_call_id = ?1 AND p.part_type = 'tool_call' \
               AND p.tool_name = 'file_read' AND m.conversation_id = ?2 \
             ORDER BY p.message_id DESC LIMIT 1",
            params![call_id, conversation_id],
            |r| r.get(0),
        )
        .ok()?;
    let args: serde_json::Value = serde_json::from_str(&tool_input).ok()?;
    Some(args.get("path")?.as_str()?.to_string())
}

/// Look up (or create) a conversation by name. Returns its id.
fn ensure_conversation(conn: &Connection, name: &str) -> Result<i64, EngineError> {
    conn.execute(
        "INSERT OR IGNORE INTO conversations (name, created_at, updated_at) \
         VALUES (?1, datetime('now'), datetime('now'))",
        [name],
    )?;
    conn.query_row(
        "SELECT conversation_id FROM conversations WHERE name = ?1",
        [name],
        |row| row.get(0),
    )
    .map_err(EngineError::from)
}

// ── Active session persistence ──────────────────────────────────────

fn read_active_session(context_dir: &Path) -> Option<String> {
    let path = context_dir.join("active_session");
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn persist_active_session(context_dir: &Path, name: &str) {
    let path = context_dir.join("active_session");
    let tmp = context_dir.join("active_session.tmp");
    if fs::write(&tmp, name).is_ok() {
        let _ = fs::rename(&tmp, &path);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::types::{ToolCall, ToolFunction};

    fn temp_engine() -> (LcmEngine, tempfile::TempDir) {
        temp_engine_with_ctx(ContextConfig::default())
    }

    #[test]
    fn tools_scope_gates_lcm_expand() {
        let (engine, _dir) = temp_engine();
        let names = |scope: ToolScope| -> Vec<&'static str> {
            engine.tools(scope).iter().map(|t| t.name()).collect()
        };
        assert_eq!(names(ToolScope::Root), ["lcm_grep", "lcm_describe"]);
        assert_eq!(
            names(ToolScope::SubAgent),
            ["lcm_grep", "lcm_describe", "lcm_expand"]
        );
    }

    /// Build a temp engine with a custom `max_tokens` budget so tests
    /// can trip the soft and hard thresholds without pumping hundreds
    /// of thousands of tokens through `push_message`.
    fn temp_engine_with_max_tokens(max_tokens: u32) -> (LcmEngine, tempfile::TempDir) {
        temp_engine_with_ctx(ContextConfig {
            max_tokens,
            ..ContextConfig::default()
        })
    }

    fn temp_engine_with_ctx(ctx: ContextConfig) -> (LcmEngine, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let engine = LcmEngine::new(
            &dir.path().join("context"),
            ctx,
            canned_summarize("summary"),
        )
        .unwrap();
        (engine, dir)
    }

    /// Engine whose externalization thresholds are 10 tokens (40
    /// bytes) for both user and tool content, so tests can trigger
    /// externalization with small payloads.
    fn temp_engine_small_threshold() -> (LcmEngine, tempfile::TempDir) {
        let mut ctx = ContextConfig {
            tool_output_tokens: 10,
            ..ContextConfig::default()
        };
        ctx.lcm.large_file_threshold = 10;
        temp_engine_with_ctx(ctx)
    }

    fn stored_content(engine: &LcmEngine, seq: i64) -> String {
        let conn = engine.conn.lock().unwrap();
        conn.query_row(
            "SELECT content FROM messages WHERE conversation_id = ?1 AND seq = ?2",
            params![engine.conversation_id, seq],
            |r| r.get(0),
        )
        .unwrap()
    }

    // ── large payload interception ──────────────────────────────────

    #[tokio::test]
    async fn oversized_user_message_is_externalized() {
        let (mut engine, dir) = temp_engine_small_threshold();
        let payload = "z".repeat(400);
        engine
            .push_message(Message::User {
                content: payload.clone(),
            })
            .await
            .unwrap();

        let content = stored_content(&engine, 0);
        assert!(content.starts_with("<file id=\"file_"));
        assert!(content.ends_with("</file>"));
        assert!(content.contains("summary"));
        assert!(!content.contains(&payload));

        let (file_id, path, byte_size, token_count): (String, String, i64, i64) = {
            let conn = engine.conn.lock().unwrap();
            conn.query_row(
                "SELECT file_id, path, byte_size, token_count FROM large_files",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap()
        };
        assert_eq!(byte_size, 400);
        assert_eq!(token_count, 100);
        // No hint: the recorded path is workspace-relative so the
        // confined file tools accept it verbatim, and the reference
        // carries it so no lcm_describe lookup is needed.
        assert_eq!(path, format!("context/lcm/payloads/{file_id}"));
        assert!(content.contains(&format!("path=\"{path}\"")));

        // Raw payload is on disk, lossless.
        let on_disk = fs::read_to_string(
            dir.path()
                .join("context")
                .join("lcm")
                .join("payloads")
                .join(&file_id),
        )
        .unwrap();
        assert_eq!(on_disk, payload);

        // Stored message token count reflects the reference, not the
        // original payload.
        let msg_tokens: i64 = {
            let conn = engine.conn.lock().unwrap();
            conn.query_row("SELECT token_count FROM messages", [], |r| r.get(0))
                .unwrap()
        };
        assert!(msg_tokens < token_count);
    }

    #[tokio::test]
    async fn sub_threshold_message_stored_verbatim() {
        let (mut engine, _dir) = temp_engine_small_threshold();
        engine
            .push_message(Message::User {
                content: "short".into(),
            })
            .await
            .unwrap();

        assert_eq!(stored_content(&engine, 0), "short");
        let count: i64 = {
            let conn = engine.conn.lock().unwrap();
            conn.query_row("SELECT COUNT(*) FROM large_files", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn tool_result_uses_file_read_path_hint() {
        let (mut engine, _dir) = temp_engine_small_threshold();
        engine
            .push_message(Message::ToolCalls {
                content: String::new(),
                calls: vec![ToolCall::new(
                    "c1".into(),
                    ToolFunction {
                        name: "file_read".parse().unwrap(),
                        arguments: r#"{"path":"data/big.json"}"#.into(),
                    },
                )],
            })
            .await
            .unwrap();

        let payload = format!("[{}]", "1,".repeat(50));
        engine
            .push_message(Message::Tool {
                call_id: "c1".into(),
                content: payload,
            })
            .await
            .unwrap();

        let content = stored_content(&engine, 1);
        assert!(content.contains("path=\"data/big.json\""));

        let (path, mime): (String, String) = {
            let conn = engine.conn.lock().unwrap();
            conn.query_row("SELECT path, mime_type FROM large_files", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap()
        };
        assert_eq!(path, "data/big.json");
        assert_eq!(mime, "application/json");
    }

    #[tokio::test]
    async fn framed_tool_result_excerpt_strips_framing() {
        use std::fmt::Write as _;

        let (mut engine, dir) = temp_engine_small_threshold();
        engine
            .push_message(Message::ToolCalls {
                content: String::new(),
                calls: vec![ToolCall::new(
                    "c1".into(),
                    ToolFunction {
                        name: "file_read".parse().unwrap(),
                        arguments: r#"{"path":"data/big.json"}"#.into(),
                    },
                )],
            })
            .await
            .unwrap();

        // The exact live shape: the safety wrapper around file_read's
        // line-numbered output and stats trailer.
        let mut body = String::new();
        for i in 0..30u32 {
            writeln!(body, "{}\t    {},", i + 3, i).unwrap();
        }
        let framed = format!(
            "<tool_output name=\"file_read\">\n\
             1\t{{\n2\t  \"users\": [\n{body}33\t    99\n34\t  ]\n35\t}}\n\n\
             (35 lines shown, 35 total, 300 bytes)\n\
             </tool_output>"
        );

        engine
            .push_message(Message::Tool {
                call_id: "c1".into(),
                content: framed.clone(),
            })
            .await
            .unwrap();

        // The mechanical excerpt sees the unframed file content:
        // no wrapper tags, no line-number columns.
        let content = stored_content(&engine, 1);
        assert!(content.contains("\"users\": ["));
        assert!(!content.contains("<tool_output"));
        assert!(!content.contains("1\t{"));

        // The disk copy stays verbatim, framing intact.
        let file_id = explore::file_id(&framed);
        let on_disk = fs::read_to_string(
            dir.path()
                .join("context")
                .join("lcm")
                .join("payloads")
                .join(&file_id),
        )
        .unwrap();
        assert_eq!(on_disk, framed);
    }

    /// A `SummarizeFn` that panics when called, proving a code path
    /// never invokes the LLM.
    fn panicking_summarize() -> SummarizeFn {
        Arc::new(|_prompt, _messages| panic!("summarizer must not be called for tool output"))
    }

    #[tokio::test]
    async fn oversized_tool_output_externalized_without_summarizer() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ContextConfig {
            tool_output_tokens: 10,
            ..ContextConfig::default()
        };
        let mut engine =
            LcmEngine::new(&dir.path().join("context"), ctx, panicking_summarize()).unwrap();

        let payload = {
            use std::fmt::Write as _;
            let mut s = String::new();
            for i in 0..100 {
                writeln!(s, "log line {i}").unwrap();
            }
            s
        };
        engine
            .push_message(Message::Tool {
                call_id: "c1".into(),
                content: payload.clone(),
            })
            .await
            .unwrap();

        let content = stored_content(&engine, 0);
        assert!(content.starts_with("<file id=\"file_"));
        assert!(content.contains("log line 0"));
        assert!(content.contains("log line 99"));
        assert!(content.contains("bytes omitted]"));
        assert!(!content.contains("log line 50\n"));

        // The excerpt is the stored exploration summary, so
        // lcm_describe surfaces it.
        let summary: String = {
            let conn = engine.conn.lock().unwrap();
            conn.query_row("SELECT exploration_summary FROM large_files", [], |r| {
                r.get(0)
            })
            .unwrap()
        };
        assert!(summary.contains("bytes omitted]"));
    }

    #[tokio::test]
    async fn tool_threshold_is_lower_than_user_threshold() {
        // 1000 tokens of content: over tool_output_tokens (10), well
        // under large_file_threshold (default 25k). The tool message
        // externalizes; the identical user message stays inline.
        let ctx = ContextConfig {
            tool_output_tokens: 10,
            ..ContextConfig::default()
        };
        let (mut engine, _dir) = temp_engine_with_ctx(ctx);
        let payload = "w".repeat(4000);

        engine
            .push_message(Message::Tool {
                call_id: "c1".into(),
                content: payload.clone(),
            })
            .await
            .unwrap();
        engine
            .push_message(Message::User {
                content: payload.clone(),
            })
            .await
            .unwrap();

        assert!(stored_content(&engine, 0).starts_with("<file id=\"file_"));
        assert_eq!(stored_content(&engine, 1), payload);
    }

    #[tokio::test]
    async fn oversized_assistant_message_not_intercepted() {
        let (mut engine, _dir) = temp_engine_small_threshold();
        let payload = "a".repeat(100);
        engine
            .push_message(Message::Assistant {
                content: payload.clone(),
            })
            .await
            .unwrap();

        assert_eq!(stored_content(&engine, 0), payload);
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
                            name: "exec".parse().unwrap(),
                            arguments: r#"{"cmd":"ls"}"#.into(),
                        },
                    ),
                    ToolCall::new(
                        "c2".into(),
                        ToolFunction {
                            name: "read".parse().unwrap(),
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
        let context_dir = dir.path().join("context");

        {
            let mut engine = LcmEngine::new(
                &context_dir,
                ContextConfig::default(),
                canned_summarize("summary"),
            )
            .unwrap();
            engine.switch_session("kitaebot").await.unwrap();
        }

        let engine = LcmEngine::new(
            &context_dir,
            ContextConfig::default(),
            canned_summarize("summary"),
        )
        .unwrap();
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
    async fn latest_positions_reports_session_tips() {
        let (mut engine, _dir) = temp_engine();
        assert!(engine.latest_positions().await.unwrap().is_empty());
        for content in ["one", "two"] {
            engine
                .push_message(Message::User {
                    content: content.into(),
                })
                .await
                .unwrap();
        }

        let tips = engine.latest_positions().await.unwrap();
        let tip = *tips.get("general").expect("session has a tip");
        // The tip is exactly where a full transcript_since would
        // advance to: nothing pending beyond it.
        let caught_up = BTreeMap::from([("general".to_string(), tip)]);
        let pending = engine.pending_distill_tokens(&caught_up).await.unwrap();
        assert!(!pending.contains_key("general"));
    }

    #[tokio::test]
    async fn pending_distill_tokens_sums_undistilled_per_session() {
        let (mut engine, _dir) = temp_engine();
        engine
            .push_message(Message::User {
                content: "g".repeat(40),
            })
            .await
            .unwrap();
        engine.switch_session("beta").await.unwrap();
        engine
            .push_message(Message::User {
                content: "b".repeat(40),
            })
            .await
            .unwrap();

        // Empty watermarks: every session counts from the start.
        let all = engine
            .pending_distill_tokens(&BTreeMap::new())
            .await
            .unwrap();
        assert!(all.get("general").copied().unwrap_or(0) > 0);
        assert!(all.get("beta").copied().unwrap_or(0) > 0);

        // Watermark at the high-water omits a caught-up session.
        let caught_up = BTreeMap::from([("beta".to_string(), 1)]);
        let pending = engine.pending_distill_tokens(&caught_up).await.unwrap();
        assert!(!pending.contains_key("beta"));
        assert!(pending.contains_key("general"));
    }

    #[tokio::test]
    async fn transcript_since_returns_span_after_watermark() {
        let (mut engine, _dir) = temp_engine();
        for i in 0..3 {
            engine
                .push_message(Message::User {
                    content: format!("m{i}"),
                })
                .await
                .unwrap();
        }

        let span = engine
            .transcript_since("general", 1, u64::MAX)
            .await
            .unwrap();
        assert_eq!(span.len(), 2);
        assert!(matches!(&span[0], Message::User { content } if content == "m1"));
        assert!(matches!(&span[1], Message::User { content } if content == "m2"));

        // At the high-water, nothing pending.
        let empty = engine
            .transcript_since("general", 3, u64::MAX)
            .await
            .unwrap();
        assert!(empty.is_empty());

        // Unknown session yields an empty span.
        let missing = engine.transcript_since("nope", 0, u64::MAX).await.unwrap();
        assert!(missing.is_empty());
    }

    #[tokio::test]
    async fn transcript_since_clamps_but_makes_progress() {
        let (mut engine, _dir) = temp_engine();
        for i in 0..3 {
            engine
                .push_message(Message::User {
                    content: format!("message number {i}"),
                })
                .await
                .unwrap();
        }

        // A zero budget still yields the head so the watermark can advance.
        let span = engine.transcript_since("general", 0, 0).await.unwrap();
        assert_eq!(span.len(), 1);
    }

    #[tokio::test]
    async fn transcript_since_clamp_cuts_mid_tail() {
        let (mut engine, _dir) = temp_engine();
        // 5 messages of 400 chars = 100 tokens each.
        for i in 0..5 {
            engine
                .push_message(Message::User {
                    content: format!("{i} ").repeat(200),
                })
                .await
                .unwrap();
        }

        // Budget 250: keeps m0..m1 (m2 would reach 300), cuts at m2.
        let span = engine.transcript_since("general", 0, 250).await.unwrap();
        assert_eq!(span.len(), 2);
        for (i, msg) in span.iter().enumerate() {
            assert!(
                matches!(msg, Message::User { content } if content.starts_with(&format!("{i} "))),
                "expected message {i}, got {msg:?}"
            );
        }

        // Resuming after the clamp returns the next slice, not all
        // the rest: the budget applies per fetch.
        let rest = engine.transcript_since("general", 2, 250).await.unwrap();
        assert_eq!(rest.len(), 2);

        // Exact boundary: a budget the first two rows sum to exactly
        // keeps both — the clamp is `>`, not `>=`.
        let exact = engine.transcript_since("general", 0, 200).await.unwrap();
        assert_eq!(exact.len(), 2);
    }

    #[tokio::test]
    async fn transcript_since_zero_budget_returns_oversized_head() {
        let (mut engine, _dir) = temp_engine();
        engine
            .push_message(Message::User {
                content: "huge".repeat(1000),
            })
            .await
            .unwrap();
        engine
            .push_message(Message::User {
                content: "tail".into(),
            })
            .await
            .unwrap();

        // The head alone exceeds any budget; progress demands it anyway.
        let span = engine.transcript_since("general", 0, 0).await.unwrap();
        assert_eq!(span.len(), 1);
        assert!(matches!(&span[0], Message::User { content } if content.starts_with("huge")));
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
                    name: "exec".parse().unwrap(),
                    arguments: r#"{"cmd":"ls"}"#.into(),
                },
            ),
            ToolCall::new(
                "c2".into(),
                ToolFunction {
                    name: "read".parse().unwrap(),
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
    async fn compact_if_urgent_no_op_below_soft_threshold() {
        // max_tokens=1000 → soft=700, hard=900. A handful of tiny
        // messages stay well below 700.
        let (mut engine, _dir) = temp_engine_with_max_tokens(1000);
        for i in 0..5 {
            engine
                .push_message(Message::User {
                    content: format!("m{i}"),
                })
                .await
                .unwrap();
        }
        let result = engine
            .compact_if_urgent(&canned_summarize("s"))
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn compact_if_urgent_blocks_above_hard_threshold() {
        // max_tokens=1000 → hard=900. 35 messages × ~51 tokens =
        // ~1785, comfortably above hard.
        let (mut engine, _dir) = temp_engine_with_max_tokens(1000);
        let filler = "x".repeat(200);
        for i in 0..35 {
            engine
                .push_message(Message::User {
                    content: format!("m{i} {filler}"),
                })
                .await
                .unwrap();
        }
        let event = engine
            .compact_if_urgent(&canned_summarize(
                "compact summary that is long enough to pass the level-1 shrink test",
            ))
            .await
            .unwrap()
            .expect("hard threshold must produce an event");
        assert!(event.before > 0);
        assert!(event.after <= event.before);
        let summary_count: i64 = engine
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM summaries", [], |r| r.get(0))
            .unwrap();
        assert!(summary_count >= 1);
    }

    /// The soft band belongs to the turn boundary: mid-turn the urgent
    /// check must leave it alone (compacting there cold-starts the
    /// prompt cache), and the between-turns pass must take it.
    #[tokio::test]
    async fn soft_band_waits_for_the_turn_boundary() {
        // max_tokens=1000 → soft=700, hard=900. 35 messages × ~21
        // tokens = ~735, sits between the two.
        let (mut engine, _dir) = temp_engine_with_max_tokens(1000);
        let filler = "x".repeat(80);
        for i in 0..35 {
            engine
                .push_message(Message::User {
                    content: format!("m{i} {filler}"),
                })
                .await
                .unwrap();
        }
        let summarize =
            canned_summarize("compact summary that is long enough to pass the level-1 shrink test");

        let mid_turn = engine.compact_if_urgent(&summarize).await.unwrap();
        assert!(mid_turn.is_none(), "soft band must not compact mid-turn");

        let event = engine
            .compact_between_turns(&summarize)
            .await
            .unwrap()
            .expect("between-turns pass must take the soft band");
        assert!(event.before > 0);
        assert!(event.after <= event.before);

        // Once shrunk below soft, the boundary pass is a no-op too.
        let again = engine.compact_between_turns(&summarize).await.unwrap();
        assert!(again.is_none());
    }

    /// Build a `SummarizeFn` that always returns the given canned
    /// summary, regardless of input.
    fn canned_summarize(summary: &'static str) -> SummarizeFn {
        Arc::new(move |_prompt, _messages| Box::pin(async move { Ok(summary.to_string()) }))
    }

    #[tokio::test]
    async fn observed_tokens_trigger_blocking_compaction() {
        // max_tokens=1000 → soft=700, hard=900. 35 messages × ~11
        // estimated tokens ≈ 385, below soft — only the observation
        // can trigger anything.
        let (mut engine, _dir) = temp_engine_with_max_tokens(1000);
        let filler = "x".repeat(40);
        for i in 0..35 {
            engine
                .push_message(Message::User {
                    content: format!("m{i} {filler}"),
                })
                .await
                .unwrap();
        }
        let summarize =
            canned_summarize("compact summary that is long enough to pass the level-1 shrink test");
        assert!(
            engine
                .compact_if_urgent(&summarize)
                .await
                .unwrap()
                .is_none()
        );

        engine.observe_tokens(950);
        let event = engine
            .compact_if_urgent(&summarize)
            .await
            .unwrap()
            .expect("observation above hard threshold must block");
        assert!(event.before > 0);

        // Cleared: the next check must not see the stale 950.
        assert!(
            engine
                .compact_if_urgent(&summarize)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn observed_tokens_reach_the_between_turns_pass() {
        // max_tokens=1000 → soft=700, hard=900. Observation of 800
        // sits between the two; the stored estimate (~385) stays
        // below soft.
        let (mut engine, _dir) = temp_engine_with_max_tokens(1000);
        let filler = "x".repeat(40);
        for i in 0..35 {
            engine
                .push_message(Message::User {
                    content: format!("m{i} {filler}"),
                })
                .await
                .unwrap();
        }
        let summarize =
            canned_summarize("compact summary that is long enough to pass the level-1 shrink test");

        engine.observe_tokens(800);
        assert!(
            engine
                .compact_if_urgent(&summarize)
                .await
                .unwrap()
                .is_none(),
            "800 observed is below hard; urgent must not fire"
        );
        let event = engine
            .compact_between_turns(&summarize)
            .await
            .unwrap()
            .expect("800 observed is above soft; boundary pass must fire");
        assert!(event.after <= event.before);
        assert!(engine.observed_tokens.is_none());
    }

    #[tokio::test]
    async fn observation_cleared_on_clear_and_switch() {
        let (mut engine, _dir) = temp_engine();

        engine.observe_tokens(500);
        assert_eq!(engine.stats().token_estimate, 500);
        engine.clear().await.unwrap();
        assert_eq!(engine.stats().token_estimate, 0);

        engine.observe_tokens(500);
        engine.switch_session("other").await.unwrap();
        assert_eq!(engine.stats().token_estimate, 0);
    }

    #[tokio::test]
    async fn force_compact_no_op_when_below_protected_tail() {
        let (mut engine, _dir) = temp_engine();
        for i in 0..5 {
            engine
                .push_message(Message::User {
                    content: format!("m{i}"),
                })
                .await
                .unwrap();
        }

        let event = engine.force_compact(&canned_summarize("s")).await.unwrap();
        assert_eq!(event.before, event.after);

        let summary_count: i64 = engine
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM summaries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(summary_count, 0);
    }

    #[tokio::test]
    async fn force_compact_creates_leaf_summary_for_eligible_messages() {
        let (mut engine, _dir) = temp_engine();
        // 32 protected + 3 eligible = 35 messages, one chunk. Each
        // message must be long enough that the escalator's level-1
        // shrink check passes ("compact" is 1 token).
        let filler = "x".repeat(200);
        for i in 0..35 {
            engine
                .push_message(Message::User {
                    content: format!("m{i} {filler}"),
                })
                .await
                .unwrap();
        }

        let event = engine
            .force_compact(&canned_summarize("compact"))
            .await
            .unwrap();
        assert!(event.before > 0);

        let conn = engine.conn.lock().unwrap();
        let summary_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM summaries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(summary_count, 1);

        let edge_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM summary_messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(edge_count, 3); // three eligible messages

        // Active context now has 1 summary + 32 protected messages.
        let item_counts: (i64, i64) = conn
            .query_row(
                "SELECT \
                    SUM(CASE WHEN item_type = 'message' THEN 1 ELSE 0 END), \
                    SUM(CASE WHEN item_type = 'summary' THEN 1 ELSE 0 END) \
                 FROM context_items",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(item_counts, (32, 1));

        // Raw messages are still in the immutable store.
        let raw: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(raw, 35);
    }

    #[tokio::test]
    async fn assemble_after_compaction_includes_summary_and_recall_guidance() {
        let (mut engine, _dir) = temp_engine();
        let filler = "x".repeat(200);
        for i in 0..35 {
            engine
                .push_message(Message::User {
                    content: format!("m{i} {filler}"),
                })
                .await
                .unwrap();
        }
        engine
            .force_compact(&canned_summarize("compact"))
            .await
            .unwrap();

        let ctx = engine.assemble("SYS").await.unwrap();
        // System(prompt + recall) + 1 summary system message + 32 protected users.
        assert_eq!(ctx.messages.len(), 1 + 1 + 32);

        match &ctx.messages[0] {
            Message::System { content } => {
                assert!(content.starts_with("SYS"));
                assert!(content.contains("Compacted History"));
                assert!(content.contains("lcm_grep"));
            }
            other => panic!("expected system, got {other:?}"),
        }
        match &ctx.messages[1] {
            Message::System { content } => {
                assert!(content.starts_with("<summary id=\"sum_"));
                assert!(content.contains("kind=\"leaf\""));
                assert!(content.contains("depth=\"0\""));
                assert!(content.contains("compact"));
                assert!(content.ends_with("</summary>"));
            }
            other => panic!("expected summary system message, got {other:?}"),
        }
    }

    /// A `ToolCalls` message carrying one `file_write` for `path`,
    /// padded so compaction chunks have real weight.
    fn write_call_message(i: usize, path: &str) -> Message {
        Message::ToolCalls {
            content: format!("writing {i} {}", "x".repeat(120)),
            calls: vec![ToolCall::new(
                format!("c{i}"),
                ToolFunction {
                    name: "file_write".parse().unwrap(),
                    arguments: format!(r#"{{"path":"{path}","content":"data"}}"#),
                },
            )],
        }
    }

    #[tokio::test]
    async fn assemble_lists_written_files_after_compaction() {
        let (mut engine, _dir) = temp_engine();
        engine
            .push_message(Message::User {
                content: "do the task".into(),
            })
            .await
            .unwrap();
        // 36 write calls after the pin; the oldest 4 leave the tail.
        // old.rs is written first and again later: it must appear once,
        // after new.rs (newest first).
        for i in 0..36 {
            let path = match i {
                0..=17 | 30 => "old.rs",
                _ => "new.rs",
            };
            engine
                .push_message(write_call_message(i, path))
                .await
                .unwrap();
        }
        engine
            .force_compact(&canned_summarize(
                "compact summary that is long enough to pass the level-1 shrink test",
            ))
            .await
            .unwrap();

        let ctx = engine.assemble("SYS").await.unwrap();
        let Message::System { content } = &ctx.messages[0] else {
            panic!("expected system message");
        };
        assert!(
            content.contains("## Files Written For This Request"),
            "{content}"
        );
        let new_at = content.find("new.rs").expect("new.rs listed");
        let old_at = content.find("old.rs").expect("old.rs listed");
        assert!(new_at < old_at, "newest first: {content}");
        assert_eq!(content.matches("old.rs").count(), 1, "deduped: {content}");
    }

    #[tokio::test]
    async fn written_files_omitted_while_calls_are_still_raw() {
        let (mut engine, _dir) = temp_engine();
        engine
            .push_message(Message::User {
                content: "do the task".into(),
            })
            .await
            .unwrap();
        engine
            .push_message(write_call_message(0, "a.rs"))
            .await
            .unwrap();

        let ctx = engine.assemble("SYS").await.unwrap();
        let Message::System { content } = &ctx.messages[0] else {
            panic!("expected system message");
        };
        assert!(!content.contains("Files Written"), "{content}");
    }

    #[tokio::test]
    async fn assemble_without_compaction_omits_recall_guidance() {
        let (mut engine, _dir) = temp_engine();
        engine
            .push_message(Message::User {
                content: "u".into(),
            })
            .await
            .unwrap();

        let ctx = engine.assemble("SYS").await.unwrap();
        match &ctx.messages[0] {
            Message::System { content } => {
                assert_eq!(content, "SYS");
                assert!(!content.contains("Compacted History"));
            }
            other => panic!("expected system, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn force_compact_runs_condensed_pass_when_multiple_leaves() {
        let (mut engine, _dir) = temp_engine();
        // Each message carries ~1000 tokens (4000 chars / 4). 25
        // eligible messages exceed leaf_chunk_tokens = 20_000, forcing
        // two leaf chunks. The two resulting depth-0 summaries form a
        // contiguous run with fanout 2 so the condensed pass kicks in.
        let big = "x".repeat(4000);
        for i in 0..(32 + 25) {
            engine
                .push_message(Message::User {
                    content: format!("m{i} {big}"),
                })
                .await
                .unwrap();
        }

        engine.force_compact(&canned_summarize("c")).await.unwrap();

        let conn = engine.conn.lock().unwrap();

        let leaf_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM summaries WHERE kind = 'leaf'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(leaf_count, 2);

        let condensed_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM summaries WHERE kind = 'condensed'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(condensed_count, 1);

        let condensed_depth: i64 = conn
            .query_row(
                "SELECT depth FROM summaries WHERE kind = 'condensed'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(condensed_depth, 1);

        let parent_edges: i64 = conn
            .query_row("SELECT COUNT(*) FROM summary_parents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(parent_edges, 2);

        // The condensed summary aggregates descendants from both leaves.
        let (descendant_count, source_msg_tokens): (i64, i64) = conn
            .query_row(
                "SELECT descendant_count, source_message_token_count \
                 FROM summaries WHERE kind = 'condensed'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(descendant_count, 25, "should sum the 25 source messages");
        assert!(source_msg_tokens > 0);

        // Active context: the condensed summary + 32 protected messages.
        let item_counts: (i64, i64) = conn
            .query_row(
                "SELECT \
                    SUM(CASE WHEN item_type = 'message' THEN 1 ELSE 0 END), \
                    SUM(CASE WHEN item_type = 'summary' THEN 1 ELSE 0 END) \
                 FROM context_items",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(item_counts, (32, 1));
    }

    #[tokio::test]
    async fn condensed_pass_skips_singleton_runs() {
        // 32 protected + 3 eligible -> 1 leaf chunk -> 1 leaf summary.
        // The condensed pass sees a single depth-0 item, which fails
        // the fanout >= 2 check, so no condensed summary is created.
        let (mut engine, _dir) = temp_engine();
        let filler = "x".repeat(200);
        for i in 0..35 {
            engine
                .push_message(Message::User {
                    content: format!("m{i} {filler}"),
                })
                .await
                .unwrap();
        }
        engine.force_compact(&canned_summarize("c")).await.unwrap();

        let condensed: i64 = engine
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM summaries WHERE kind = 'condensed'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(condensed, 0);
    }
}
